//! Show the resolved effective configuration

use crate::commands::Command;
use crate::config::{ConfigFile, TestrConfig, TimeoutSetting};
use crate::error::Result;
use crate::ordering::TestOrder;
use crate::ui::UI;
use std::path::Path;
use std::time::Duration;

/// Origin of a resolved configuration value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Cli,
    Profile(String),
    Config,
    Default,
}

impl Source {
    fn label(&self) -> String {
        match self {
            Source::Cli => "cli".to_string(),
            Source::Profile(name) => format!("profile:{}", name),
            Source::Config => "config".to_string(),
            Source::Default => "default".to_string(),
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
    /// Active profile name (from `--profile` / `INQ_PROFILE`).
    pub profile: Option<String>,
    /// When set, list available profiles and exit instead of resolving.
    pub list_profiles: bool,
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
            profile: None,
            list_profiles: false,
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

        let loaded = ConfigFile::find_in_directory(base).ok();

        if self.list_profiles {
            return run_list_profiles(ui, &loaded);
        }

        match &loaded {
            Some((_, path)) => {
                ui.output(&format!("Config file: {}", path.display()))?;
            }
            None => {
                ui.output("Config file: (none found)")?;
            }
        }
        ui.output(&format!("Working directory: {}", base.display()))?;

        // Determine the active profile and the resolved config. When no
        // config file is present, fall back to defaults so callers can
        // still see CLI overrides resolved.
        let (resolved, active_profile, profile_fields) = match &loaded {
            Some((cfg, _)) => {
                let (resolved, active) = cfg.resolve(self.profile.as_deref())?;
                let fields = cfg.fields_from_profile(active.as_deref());
                (resolved, active, fields)
            }
            None => (
                TestrConfig::default(),
                None,
                std::collections::HashSet::new(),
            ),
        };

        if let Some(ref name) = active_profile {
            ui.output(&format!("Active profile: {}", name))?;
        }
        ui.output("")?;

        let config_or_profile = |key: &str| -> Source {
            if profile_fields.contains(key) {
                Source::Profile(active_profile.clone().unwrap_or_default())
            } else {
                Source::Config
            }
        };

        if !resolved.test_command.is_empty() {
            print_value(
                ui,
                "test_command",
                &resolved.test_command,
                config_or_profile("test_command"),
            )?;
        } else {
            print_unset(ui, "test_command", "(unset)")?;
        }

        print_optional_string(
            ui,
            "test_id_option",
            &resolved.test_id_option,
            config_or_profile("test_id_option"),
        )?;
        print_optional_string(
            ui,
            "test_list_option",
            &resolved.test_list_option,
            config_or_profile("test_list_option"),
        )?;
        print_optional_string(
            ui,
            "test_id_list_default",
            &resolved.test_id_list_default,
            config_or_profile("test_id_list_default"),
        )?;
        print_optional_string(
            ui,
            "test_run_concurrency",
            &resolved.test_run_concurrency,
            config_or_profile("test_run_concurrency"),
        )?;
        print_optional_string(
            ui,
            "filter_tags",
            &resolved.filter_tags,
            config_or_profile("filter_tags"),
        )?;
        print_optional_string(
            ui,
            "group_regex",
            &resolved.group_regex,
            config_or_profile("group_regex"),
        )?;
        print_optional_string(
            ui,
            "instance_provision",
            &resolved.instance_provision,
            config_or_profile("instance_provision"),
        )?;
        print_optional_string(
            ui,
            "instance_execute",
            &resolved.instance_execute,
            config_or_profile("instance_execute"),
        )?;
        print_optional_string(
            ui,
            "instance_dispose",
            &resolved.instance_dispose,
            config_or_profile("instance_dispose"),
        )?;

        let (test_timeout_str, src) = resolve_timeout(
            self.test_timeout.as_deref(),
            &resolved.test_timeout,
            config_or_profile("test_timeout"),
        )?;
        print_value(ui, "test_timeout", &test_timeout_str, src)?;

        let (max_duration_str, src) = resolve_timeout(
            self.max_duration.as_deref(),
            &resolved.max_duration,
            config_or_profile("max_duration"),
        )?;
        print_value(ui, "max_duration", &max_duration_str, src)?;

        let (no_output_str, src) = resolve_no_output_timeout(
            self.no_output_timeout.as_deref(),
            &resolved.no_output_timeout,
            config_or_profile("no_output_timeout"),
        )?;
        print_value(ui, "no_output_timeout", &no_output_str, src)?;

        let (order_str, src) = resolve_order(
            self.order.as_deref(),
            &resolved.test_order,
            config_or_profile("test_order"),
        )?;
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

fn run_list_profiles(
    ui: &mut dyn UI,
    loaded: &Option<(ConfigFile, std::path::PathBuf)>,
) -> Result<i32> {
    let Some((cfg, path)) = loaded else {
        ui.output("Config file: (none found)")?;
        return Ok(0);
    };
    ui.output(&format!("Config file: {}", path.display()))?;
    let names = cfg.profile_names();
    if names.is_empty() {
        ui.output("No profiles defined")?;
    } else {
        ui.output("Profiles:")?;
        for name in names {
            ui.output(&format!("  {}", name))?;
        }
    }
    if let Some(ref dp) = cfg.default_profile {
        ui.output(&format!("default_profile: {}", dp))?;
    }
    Ok(0)
}

fn print_value(ui: &mut dyn UI, key: &str, value: &str, src: Source) -> Result<()> {
    ui.output(&format!("{}: {} [{}]", key, value, src.label()))
}

fn print_unset(ui: &mut dyn UI, key: &str, placeholder: &str) -> Result<()> {
    ui.output(&format!("{}: {}", key, placeholder))
}

fn print_optional_string(
    ui: &mut dyn UI,
    key: &str,
    value: &Option<String>,
    src: Source,
) -> Result<()> {
    match value {
        Some(v) => print_value(ui, key, v, src),
        None => print_unset(ui, key, "(unset)"),
    }
}

fn resolve_timeout(
    cli: Option<&str>,
    config_value: &Option<String>,
    config_source: Source,
) -> Result<(String, Source)> {
    if let Some(s) = cli {
        let parsed = TimeoutSetting::parse(s)?;
        return Ok((format_timeout(&parsed), Source::Cli));
    }
    match config_value {
        Some(s) => {
            TimeoutSetting::parse(s)?;
            Ok((s.trim().to_string(), config_source))
        }
        None => Ok((format_timeout(&TimeoutSetting::Disabled), Source::Default)),
    }
}

fn resolve_no_output_timeout(
    cli: Option<&str>,
    config_value: &Option<String>,
    config_source: Source,
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
                Ok(("disabled".to_string(), config_source))
            } else {
                crate::config::parse_duration_string(trimmed)?;
                Ok((trimmed.to_string(), config_source))
            }
        }
        None => Ok(("disabled".to_string(), Source::Default)),
    }
}

fn resolve_order(
    cli: Option<&str>,
    config_value: &Option<String>,
    config_source: Source,
) -> Result<(String, Source)> {
    if let Some(s) = cli {
        let parsed: TestOrder = s.parse()?;
        return Ok((parsed.as_str(), Source::Cli));
    }
    match config_value {
        Some(s) => {
            let parsed: TestOrder = s.parse()?;
            Ok((parsed.as_str(), config_source))
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
            profile: None,
            list_profiles: false,
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
            profile: None,
            list_profiles: false,
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

    #[test]
    fn profile_overlay_is_labelled() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("inquest.toml"),
            r#"
test_command = "echo"
test_timeout = "1m"

[profiles.ci]
test_timeout = "5m"
"#,
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = ConfigCommand {
            base_path: Some(temp.path().to_string_lossy().to_string()),
            profile: Some("ci".to_string()),
            ..ConfigCommand::new(None)
        };
        cmd.execute(&mut ui).unwrap();

        let joined = ui.output.join("\n");
        assert!(joined.contains("Active profile: ci"), "got: {}", joined);
        assert!(
            joined.contains("test_timeout: 5m [profile:ci]"),
            "got: {}",
            joined
        );
        // Untouched base field shows [config], not [profile:ci].
        assert!(
            joined.contains("test_command: echo [config]"),
            "got: {}",
            joined
        );
    }

    #[test]
    fn list_profiles_lists_names() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("inquest.toml"),
            r#"
test_command = "echo"
default_profile = "ci"

[profiles.ci]
test_timeout = "5m"

[profiles.nightly]
test_timeout = "30m"
"#,
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = ConfigCommand {
            base_path: Some(temp.path().to_string_lossy().to_string()),
            list_profiles: true,
            ..ConfigCommand::new(None)
        };
        cmd.execute(&mut ui).unwrap();

        let joined = ui.output.join("\n");
        assert!(joined.contains("Profiles:"), "got: {}", joined);
        assert!(joined.contains("  ci"), "got: {}", joined);
        assert!(joined.contains("  nightly"), "got: {}", joined);
        assert!(joined.contains("default_profile: ci"), "got: {}", joined);
    }

    #[test]
    fn list_profiles_when_none_defined() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("inquest.toml"), r#"test_command = "echo""#).unwrap();

        let mut ui = TestUI::new();
        let cmd = ConfigCommand {
            base_path: Some(temp.path().to_string_lossy().to_string()),
            list_profiles: true,
            ..ConfigCommand::new(None)
        };
        cmd.execute(&mut ui).unwrap();

        let joined = ui.output.join("\n");
        assert!(joined.contains("No profiles defined"), "got: {}", joined);
    }

    #[test]
    fn unknown_profile_errors() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("inquest.toml"),
            r#"
test_command = "echo"

[profiles.ci]
test_timeout = "5m"
"#,
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = ConfigCommand {
            base_path: Some(temp.path().to_string_lossy().to_string()),
            profile: Some("nope".to_string()),
            ..ConfigCommand::new(None)
        };
        let err = cmd.execute(&mut ui).unwrap_err().to_string();
        assert_eq!(
            err,
            "Configuration error: profile 'nope' is not defined; available profiles: ci"
        );
    }
}
