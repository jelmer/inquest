//! Configuration file parsing and handling
//!
//! Supports three configuration file formats:
//! - `inquest.toml` or `.inquest.toml` - TOML format (preferred)
//! - `.testr.conf` - legacy INI format with a `[DEFAULT]` section
//!
//! TOML configs may declare named **profiles** under `[profiles.<name>]` to
//! switch between alternative sets of values (e.g. tighter timeouts for CI).
//! Top-level fields form the implicit "base" layer; a selected profile
//! overlays its set fields on top of that base. See [`ConfigFile`].

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
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

/// Maximum number of times a test process can be restarted (due to per-test
/// timeouts or crashes) before giving up. Prevents infinite restart loops when
/// many tests hang or the runner keeps crashing.
pub const MAX_TEST_RESTARTS: usize = 10;

/// Multiplier for slow test warnings: warn if a test takes longer than this
/// multiple of its historical average duration.
pub const SLOW_TEST_WARNING_MULTIPLIER: f64 = 3.0;

/// Minimum absolute duration for a test to be considered for slow-test warnings.
/// Tests faster than this are treated as noise even if they exceed the multiplier,
/// since small absolute variations produce huge ratios on sub-second tests.
pub const SLOW_TEST_WARNING_MIN_DURATION: std::time::Duration = std::time::Duration::from_millis(2);

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
                        "invalid duration suffix '{}' in '{}' (use 's', 'm', or 'h', e.g. '30s', '5m', '1h')",
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

/// Reserved profile name that refers to the base (top-level) layer.
pub const DEFAULT_PROFILE_NAME: &str = "default";

/// Environment variable used to select a profile when no `--profile` CLI flag
/// is given.
pub const PROFILE_ENV_VAR: &str = "INQ_PROFILE";

/// Environment variable that, when set to a non-empty value other than `0`,
/// suppresses inq's own progress bars. Inq sets this on every test child
/// process so nested `inq` invocations stay quiet too.
pub const NO_PROGRESS_ENV_VAR: &str = "INQ_NO_PROGRESS";

/// Returns true when progress bars should be suppressed.
///
/// Progress is disabled when either:
///   * [`disable_progress_in_process`] has been called (used by the test
///     suite to keep `cargo test` output clean), or
///   * [`NO_PROGRESS_ENV_VAR`] is set to a non-empty value other than `0`.
pub fn progress_disabled() -> bool {
    if PROGRESS_DISABLED_FLAG.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    progress_disabled_for(std::env::var(NO_PROGRESS_ENV_VAR).ok().as_deref())
}

/// Process-wide kill switch for progress bars. Once set, never cleared.
/// Inquest's own integration tests flip this so progress output doesn't
/// pollute `cargo test`'s captured stdout.
static PROGRESS_DISABLED_FLAG: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Disable inquest progress bars for the rest of this process. Idempotent
/// and thread-safe; intended for tests that run [`crate::commands::RunCommand`]
/// or the executor in-process and don't want progress chrome in their
/// captured stdout. Equivalent to setting `INQ_NO_PROGRESS=1`, except it
/// can't be overridden by an explicit `INQ_NO_PROGRESS=0`.
pub fn disable_progress_in_process() {
    PROGRESS_DISABLED_FLAG.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Pure helper: decide whether the supplied env value disables progress.
/// `None` (unset) leaves progress enabled; an empty string or `"0"` are
/// treated as not-set so `INQ_NO_PROGRESS=0` is an opt-out.
fn progress_disabled_for(value: Option<&str>) -> bool {
    matches!(value, Some(v) if !v.is_empty() && v != "0")
}

/// A single layer of overlayable settings — every field is optional so it can
/// be merged on top of another layer. Used for profile overlays and as the
/// shape that `[profiles.<name>]` tables deserialize into.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct ConfigLayer {
    /// Command line to run to execute tests
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_command: Option<String>,
    /// The value to substitute into test_command when specific test ids should be run
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_id_option: Option<String>,
    /// The option to use to cause the test runner to report test ids it would run
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_list_option: Option<String>,
    /// The value to use for $IDLIST when no specific test ids are being run
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_id_list_default: Option<String>,
    /// Optional call out to establish concurrency
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_run_concurrency: Option<String>,
    /// Tags which should be used to filter test counts
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_tags: Option<String>,
    /// If set, group tests by the matched section of the test id
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_regex: Option<String>,
    /// Per-test timeout (e.g. "5m", "auto", "disabled")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_timeout: Option<String>,
    /// Overall run timeout
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_duration: Option<String>,
    /// No-output timeout
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_output_timeout: Option<String>,
    /// Provision one or more test run environments
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_provision: Option<String>,
    /// Execute a test runner process in a given environment
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_execute: Option<String>,
    /// Dispose of one or more test running environments
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_dispose: Option<String>,
    /// Default test ordering strategy
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_order: Option<String>,
}

impl ConfigLayer {
    /// Validate per-layer syntactic constraints (durations parse, regex
    /// compiles, `test_order` names a known strategy). Cross-field checks
    /// such as `$IDOPTION` requiring `test_id_option` are deferred to the
    /// resolved config, so that a profile may legally lack `test_command`
    /// when the base supplies it.
    fn validate_syntax(&self, context: &str) -> Result<()> {
        if let Some(ref pattern) = self.group_regex {
            regex::Regex::new(pattern).map_err(|e| {
                Error::Config(format!(
                    "{}: invalid group_regex pattern '{}': {}",
                    context, pattern, e
                ))
            })?;
        }
        if let Some(ref s) = self.test_order {
            s.parse::<crate::ordering::TestOrder>()
                .map_err(|e| Error::Config(format!("{}: {}", context, prefix_strip(&e))))?;
        }
        if let Some(ref s) = self.test_timeout {
            TimeoutSetting::parse(s).map_err(|e| {
                Error::Config(format!("{}: test_timeout: {}", context, prefix_strip(&e)))
            })?;
        }
        if let Some(ref s) = self.max_duration {
            TimeoutSetting::parse(s).map_err(|e| {
                Error::Config(format!("{}: max_duration: {}", context, prefix_strip(&e)))
            })?;
        }
        if let Some(ref s) = self.no_output_timeout {
            let trimmed = s.trim();
            if !(trimmed.is_empty() || trimmed.eq_ignore_ascii_case("disabled")) {
                parse_duration_string(trimmed).map_err(|e| {
                    Error::Config(format!(
                        "{}: no_output_timeout: {}",
                        context,
                        prefix_strip(&e)
                    ))
                })?;
            }
        }
        Ok(())
    }

    /// Apply `other` on top of `self` in place. `Some(_)` values in `other`
    /// (including `Some("")`) override `self`; `None` leaves `self` intact.
    fn overlay(&mut self, other: &ConfigLayer) {
        macro_rules! overlay_field {
            ($field:ident) => {
                if let Some(ref v) = other.$field {
                    self.$field = Some(v.clone());
                }
            };
        }
        overlay_field!(test_command);
        overlay_field!(test_id_option);
        overlay_field!(test_list_option);
        overlay_field!(test_id_list_default);
        overlay_field!(test_run_concurrency);
        overlay_field!(filter_tags);
        overlay_field!(group_regex);
        overlay_field!(test_timeout);
        overlay_field!(max_duration);
        overlay_field!(no_output_timeout);
        overlay_field!(instance_provision);
        overlay_field!(instance_execute);
        overlay_field!(instance_dispose);
        overlay_field!(test_order);
    }
}

/// A parsed configuration file: a base layer plus optional named profiles.
///
/// Top-level TOML fields populate `base` (the existing flat layout).
/// `[profiles.<name>]` tables define overlayable variants. Use
/// [`ConfigFile::resolve`] to apply a selected profile and produce a
/// [`TestrConfig`] suitable for downstream consumers.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigFile {
    /// The base / default layer — top-level fields in the TOML file.
    #[serde(flatten)]
    pub base: ConfigLayer,
    /// Name of the profile applied when no `--profile` flag and no
    /// `INQ_PROFILE` env var are set. Must reference a defined profile or
    /// the reserved name "default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    /// Named profile layers. `BTreeMap` so listings and error messages are
    /// deterministic.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, ConfigLayer>,
}

impl ConfigFile {
    /// Load a `ConfigFile` from a path. TOML parses with full profile support;
    /// legacy `.testr.conf` parses into a `ConfigFile` with no profiles and
    /// no `default_profile`.
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

    /// Search `dir` for a config file (in `CONFIG_FILE_NAMES` priority order)
    /// and load it. Returns the parsed file and its path.
    pub fn find_in_directory(dir: &Path) -> Result<(Self, std::path::PathBuf)> {
        for name in CONFIG_FILE_NAMES {
            let path = dir.join(name);
            if path.exists() {
                let cfg = Self::load_from_file(&path)?;
                return Ok((cfg, path));
            }
        }
        Err(Error::Config(format!(
            "No configuration file found (looked for {})",
            CONFIG_FILE_NAMES.join(", ")
        )))
    }

    /// Parse a TOML config string with full profile support.
    pub fn parse_toml(contents: &str) -> Result<Self> {
        let cfg: ConfigFile = toml::from_str(contents)
            .map_err(|e| Error::Config(format!("Failed to parse TOML config: {}", e)))?;
        cfg.validate_syntax()?;
        Ok(cfg)
    }

    /// Parse legacy INI (`.testr.conf`) into a `ConfigFile`. The INI format
    /// has no profile support; the parsed values populate the base layer.
    pub fn parse_ini(contents: &str) -> Result<Self> {
        let base = parse_ini_base(contents)?;
        let cfg = ConfigFile {
            base,
            default_profile: None,
            profiles: BTreeMap::new(),
        };
        cfg.validate_syntax()?;
        Ok(cfg)
    }

    /// Per-layer syntactic validation plus profile-name validation. This
    /// runs without merging, so `inq config --list-profiles` succeeds even
    /// when the base lacks a `test_command`.
    fn validate_syntax(&self) -> Result<()> {
        self.base.validate_syntax("base config")?;
        for (name, layer) in &self.profiles {
            validate_profile_name(name)?;
            layer.validate_syntax(&format!("profile '{}'", name))?;
        }
        if let Some(ref name) = self.default_profile {
            if name != DEFAULT_PROFILE_NAME && !self.profiles.contains_key(name) {
                return Err(Error::Config(format!(
                    "default_profile '{}' is not defined; {}",
                    name,
                    self.available_profiles_message()
                )));
            }
        }
        Ok(())
    }

    /// Return profile names in deterministic order.
    pub fn profile_names(&self) -> Vec<&str> {
        self.profiles.keys().map(|s| s.as_str()).collect()
    }

    /// Resolve to a `TestrConfig` by overlaying the selected profile on the
    /// base layer.
    ///
    /// Selection precedence (highest first):
    ///   1. the explicit `selected` argument (`--profile` CLI flag or
    ///      `INQ_PROFILE` env var resolved by the caller).
    ///   2. the file's `default_profile` field, if present.
    ///   3. base only (no overlay).
    ///
    /// Returns `(TestrConfig, active_profile_name)`. The active profile name
    /// is `None` when no profile was applied.
    pub fn resolve(&self, selected: Option<&str>) -> Result<(TestrConfig, Option<String>)> {
        let active = match selected {
            Some(name) => Some(name.to_string()),
            None => self.default_profile.clone(),
        };

        let active = match active {
            None => None,
            Some(ref name) if name == DEFAULT_PROFILE_NAME => None,
            Some(name) => {
                if !self.profiles.contains_key(&name) {
                    return Err(Error::Config(format!(
                        "profile '{}' is not defined; {}",
                        name,
                        self.available_profiles_message()
                    )));
                }
                Some(name)
            }
        };

        let mut merged = self.base.clone();
        if let Some(ref name) = active {
            merged.overlay(&self.profiles[name]);
        }

        let resolved = TestrConfig::from_resolved_layer(merged)?;
        Ok((resolved, active))
    }

    /// Mark which fields in the resolved layer originated from the active
    /// profile (vs. the base layer). Used by `inq config` for source
    /// annotation. Returns a set of field names that the profile overrode.
    pub fn fields_from_profile(
        &self,
        profile: Option<&str>,
    ) -> std::collections::HashSet<&'static str> {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        let Some(name) = profile else {
            return set;
        };
        if name == DEFAULT_PROFILE_NAME {
            return set;
        }
        let Some(layer) = self.profiles.get(name) else {
            return set;
        };
        macro_rules! check {
            ($field:ident) => {
                if layer.$field.is_some() {
                    set.insert(stringify!($field));
                }
            };
        }
        check!(test_command);
        check!(test_id_option);
        check!(test_list_option);
        check!(test_id_list_default);
        check!(test_run_concurrency);
        check!(filter_tags);
        check!(group_regex);
        check!(test_timeout);
        check!(max_duration);
        check!(no_output_timeout);
        check!(instance_provision);
        check!(instance_execute);
        check!(instance_dispose);
        check!(test_order);
        set
    }

    fn available_profiles_message(&self) -> String {
        if self.profiles.is_empty() {
            "no profiles are defined".to_string()
        } else {
            format!("available profiles: {}", self.profile_names().join(", "))
        }
    }
}

/// Strip a redundant `Configuration error: ` prefix from a nested
/// `Error::Config` so we can re-wrap without duplicating the leader.
fn prefix_strip(e: &Error) -> String {
    let s = e.to_string();
    s.strip_prefix("Configuration error: ")
        .unwrap_or(&s)
        .to_string()
}

fn validate_profile_name(name: &str) -> Result<()> {
    if name == DEFAULT_PROFILE_NAME {
        return Err(Error::Config(format!(
            "[profiles.{}] is reserved; the top-level table is the implicit default",
            DEFAULT_PROFILE_NAME
        )));
    }
    if name.is_empty() {
        return Err(Error::Config("profile name must not be empty".to_string()));
    }
    if name.starts_with('_') {
        return Err(Error::Config(format!(
            "invalid profile name '{}': must not start with '_'",
            name
        )));
    }
    if name
        .chars()
        .any(|c| c == '.' || c == '/' || c.is_whitespace())
    {
        return Err(Error::Config(format!(
            "invalid profile name '{}': must not contain '.', '/', or whitespace",
            name
        )));
    }
    Ok(())
}

fn parse_ini_base(contents: &str) -> Result<ConfigLayer> {
    let ini: HashMap<String, HashMap<String, String>> = serde_ini::from_str(contents)
        .map_err(|e| Error::Config(format!("Failed to parse .testr.conf: {}", e)))?;
    let default = ini
        .get("DEFAULT")
        .ok_or_else(|| Error::Config("No [DEFAULT] section in .testr.conf".to_string()))?;
    Ok(ConfigLayer {
        test_command: default.get("test_command").cloned(),
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
        test_order: default.get("test_order").cloned(),
    })
}

/// Configuration loaded from inquest.toml or .testr.conf (post-resolution).
///
/// This is the shape downstream code consumes — produced by
/// [`ConfigFile::resolve`] after applying any active profile overlay. For
/// backward compatibility, [`TestrConfig::find_in_directory`],
/// [`TestrConfig::load_from_file`], [`TestrConfig::parse_toml`], and
/// [`TestrConfig::parse_ini`] still load and return a base-only resolved
/// config.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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

    /// Default test ordering strategy. One of: "auto", "discovery",
    /// "alphabetical", "failing-first", "spread", "shuffle[:<seed>]",
    /// "slowest-first", "fastest-first", "frequent-failing-first".
    pub test_order: Option<String>,
}

impl TestrConfig {
    /// Load configuration from a config file, detecting format from the file
    /// extension. Returns a base-only resolved config (no profile overlay).
    /// For profile-aware loading, use [`ConfigFile::load_from_file`].
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let cfg = ConfigFile::load_from_file(path)?;
        let (resolved, _active) = cfg.resolve(None)?;
        Ok(resolved)
    }

    /// Find and load the configuration file from a directory. Returns a
    /// base-only resolved config. For profile-aware loading use
    /// [`ConfigFile::find_in_directory`] followed by [`ConfigFile::resolve`].
    pub fn find_in_directory(dir: &Path) -> Result<(Self, std::path::PathBuf)> {
        let (cfg, path) = ConfigFile::find_in_directory(dir)?;
        let (resolved, _active) = cfg.resolve(None)?;
        Ok((resolved, path))
    }

    /// Parse a TOML string and resolve to a base-only config (no profile
    /// overlay). For profile-aware parsing use [`ConfigFile::parse_toml`].
    pub fn parse_toml(contents: &str) -> Result<Self> {
        let cfg = ConfigFile::parse_toml(contents)?;
        let (resolved, _active) = cfg.resolve(None)?;
        Ok(resolved)
    }

    /// Serialize the configuration to a TOML string. Fields with `None` values
    /// are omitted.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self)
            .map_err(|e| Error::Config(format!("Failed to serialize TOML config: {}", e)))
    }

    /// Parse a legacy `.testr.conf` (INI) string into a resolved config.
    pub fn parse_ini(contents: &str) -> Result<Self> {
        let cfg = ConfigFile::parse_ini(contents)?;
        let (resolved, _active) = cfg.resolve(None)?;
        Ok(resolved)
    }

    /// Parse configuration from an INI string (legacy .testr.conf format)
    ///
    /// Alias for `parse_ini` for backward compatibility.
    pub fn parse(contents: &str) -> Result<Self> {
        Self::parse_ini(contents)
    }

    /// Build a resolved `TestrConfig` from a fully-overlaid `ConfigLayer`.
    /// Performs cross-field validation (`test_command` non-empty,
    /// `$IDOPTION` ⇒ `test_id_option` set, etc.) on the merged result.
    pub(crate) fn from_resolved_layer(layer: ConfigLayer) -> Result<Self> {
        let test_command = layer.test_command.unwrap_or_default();
        if test_command.is_empty() {
            return Err(Error::Config("test_command cannot be empty".to_string()));
        }
        if test_command.contains("$IDOPTION") && layer.test_id_option.is_none() {
            return Err(Error::Config(
                "test_command uses $IDOPTION but test_id_option is not configured".to_string(),
            ));
        }
        if test_command.contains("$LISTOPT") && layer.test_list_option.is_none() {
            return Err(Error::Config(
                "test_command uses $LISTOPT but test_list_option is not configured".to_string(),
            ));
        }
        Ok(TestrConfig {
            test_command,
            test_id_option: layer.test_id_option,
            test_list_option: layer.test_list_option,
            test_id_list_default: layer.test_id_list_default,
            test_run_concurrency: layer.test_run_concurrency,
            filter_tags: layer.filter_tags,
            group_regex: layer.group_regex,
            test_timeout: layer.test_timeout,
            max_duration: layer.max_duration,
            no_output_timeout: layer.no_output_timeout,
            instance_provision: layer.instance_provision,
            instance_execute: layer.instance_execute,
            instance_dispose: layer.instance_dispose,
            test_order: layer.test_order,
        })
    }

    /// Parse the configured test ordering, or return the default if unset.
    pub fn parsed_test_order(&self) -> Result<crate::ordering::TestOrder> {
        match &self.test_order {
            None => Ok(crate::ordering::TestOrder::Discovery),
            Some(s) => s.parse(),
        }
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

    /// Parse the configured `filter_tags` into a list of tag entries.
    ///
    /// Tags are space-separated. A leading `!` marks an exclusion; otherwise
    /// the entry is an inclusion.
    pub fn parsed_filter_tags(&self) -> Vec<String> {
        match &self.filter_tags {
            None => Vec::new(),
            Some(s) => s.split_whitespace().map(|t| t.to_string()).collect(),
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
    fn test_progress_disabled_for() {
        assert!(!progress_disabled_for(None));
        assert!(!progress_disabled_for(Some("")));
        assert!(!progress_disabled_for(Some("0")));
        assert!(progress_disabled_for(Some("1")));
        assert!(progress_disabled_for(Some("true")));
        assert!(progress_disabled_for(Some("yes")));
    }

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
    fn test_parse_duration_string_fractional() {
        assert_eq!(
            parse_duration_string("1.5m").unwrap(),
            Duration::from_secs(90)
        );
        assert_eq!(
            parse_duration_string("0.5h").unwrap(),
            Duration::from_secs(1800)
        );
    }

    #[test]
    fn test_parse_duration_string_zero_is_error() {
        assert!(parse_duration_string("0").is_err());
        assert!(parse_duration_string("0s").is_err());
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

    #[test]
    fn test_parsed_filter_tags_unset() {
        let config = TestrConfig::parse_toml(r#"test_command = "echo""#).unwrap();
        assert_eq!(config.parsed_filter_tags(), Vec::<String>::new());
    }

    #[test]
    fn test_parsed_filter_tags_space_separated() {
        let config_str = r#"
test_command = "echo"
filter_tags = "worker-0  !slow"
"#;
        let config = TestrConfig::parse_toml(config_str).unwrap();
        assert_eq!(
            config.parsed_filter_tags(),
            vec!["worker-0".to_string(), "!slow".to_string()]
        );
    }

    // ----- Profile tests -----

    #[test]
    fn profile_overlay_overrides_base_fields() {
        let cfg = ConfigFile::parse_toml(
            r#"
test_command = "python -m test"
test_timeout = "1m"
test_order = "discovery"

[profiles.ci]
test_timeout = "5m"
test_order = "alphabetical"
"#,
        )
        .unwrap();

        let (resolved, active) = cfg.resolve(Some("ci")).unwrap();
        assert_eq!(active.as_deref(), Some("ci"));
        assert_eq!(resolved.test_command, "python -m test");
        assert_eq!(resolved.test_timeout.as_deref(), Some("5m"));
        assert_eq!(resolved.test_order.as_deref(), Some("alphabetical"));
    }

    #[test]
    fn profile_overlay_leaves_unset_fields_alone() {
        let cfg = ConfigFile::parse_toml(
            r#"
test_command = "python -m test"
filter_tags = "worker-0"

[profiles.ci]
test_timeout = "5m"
"#,
        )
        .unwrap();

        let (resolved, _) = cfg.resolve(Some("ci")).unwrap();
        assert_eq!(resolved.filter_tags.as_deref(), Some("worker-0"));
        assert_eq!(resolved.test_timeout.as_deref(), Some("5m"));
    }

    #[test]
    fn profile_empty_string_overrides_base() {
        // A profile setting `filter_tags = ""` should clear the base value,
        // not be treated as "unset, use base".
        let cfg = ConfigFile::parse_toml(
            r#"
test_command = "echo"
filter_tags = "worker-0"

[profiles.nightly]
filter_tags = ""
"#,
        )
        .unwrap();

        let (resolved, _) = cfg.resolve(Some("nightly")).unwrap();
        assert_eq!(resolved.filter_tags.as_deref(), Some(""));
        assert_eq!(resolved.parsed_filter_tags(), Vec::<String>::new());
    }

    #[test]
    fn profile_resolve_default_returns_base() {
        let cfg = ConfigFile::parse_toml(
            r#"
test_command = "echo"
test_timeout = "1m"

[profiles.ci]
test_timeout = "5m"
"#,
        )
        .unwrap();

        let (resolved, active) = cfg.resolve(None).unwrap();
        assert!(active.is_none());
        assert_eq!(resolved.test_timeout.as_deref(), Some("1m"));
    }

    #[test]
    fn profile_default_keyword_is_base() {
        let cfg = ConfigFile::parse_toml(
            r#"
test_command = "echo"
test_timeout = "1m"

[profiles.ci]
test_timeout = "5m"
"#,
        )
        .unwrap();

        let (resolved, active) = cfg.resolve(Some("default")).unwrap();
        assert!(active.is_none());
        assert_eq!(resolved.test_timeout.as_deref(), Some("1m"));
    }

    #[test]
    fn profile_unknown_name_is_an_error() {
        let cfg = ConfigFile::parse_toml(
            r#"
test_command = "echo"

[profiles.ci]
test_timeout = "5m"
"#,
        )
        .unwrap();

        let err = cfg.resolve(Some("nope")).unwrap_err().to_string();
        assert_eq!(
            err,
            "Configuration error: profile 'nope' is not defined; available profiles: ci"
        );
    }

    #[test]
    fn profile_unknown_name_with_no_profiles_defined() {
        let cfg = ConfigFile::parse_toml(r#"test_command = "echo""#).unwrap();
        let err = cfg.resolve(Some("nope")).unwrap_err().to_string();
        assert_eq!(
            err,
            "Configuration error: profile 'nope' is not defined; no profiles are defined"
        );
    }

    #[test]
    fn profiles_default_is_reserved() {
        let err = ConfigFile::parse_toml(
            r#"
test_command = "echo"

[profiles.default]
test_timeout = "5m"
"#,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "Configuration error: [profiles.default] is reserved; the top-level table is the implicit default"
        );
    }

    #[test]
    fn invalid_profile_name_is_rejected() {
        let err = ConfigFile::parse_toml(
            r#"
test_command = "echo"

[profiles."has space"]
test_timeout = "5m"
"#,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "Configuration error: invalid profile name 'has space': must not contain '.', '/', or whitespace"
        );
    }

    #[test]
    fn profile_name_starting_with_underscore_is_rejected() {
        let err = ConfigFile::parse_toml(
            r#"
test_command = "echo"

[profiles._internal]
test_timeout = "5m"
"#,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "Configuration error: invalid profile name '_internal': must not start with '_'"
        );
    }

    #[test]
    fn default_profile_must_exist() {
        let err = ConfigFile::parse_toml(
            r#"
test_command = "echo"
default_profile = "nope"

[profiles.ci]
test_timeout = "5m"
"#,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "Configuration error: default_profile 'nope' is not defined; available profiles: ci"
        );
    }

    #[test]
    fn default_profile_applies_when_no_selection() {
        let cfg = ConfigFile::parse_toml(
            r#"
test_command = "echo"
test_timeout = "1m"
default_profile = "ci"

[profiles.ci]
test_timeout = "5m"
"#,
        )
        .unwrap();

        let (resolved, active) = cfg.resolve(None).unwrap();
        assert_eq!(active.as_deref(), Some("ci"));
        assert_eq!(resolved.test_timeout.as_deref(), Some("5m"));
    }

    #[test]
    fn explicit_selection_overrides_default_profile() {
        let cfg = ConfigFile::parse_toml(
            r#"
test_command = "echo"
default_profile = "ci"

[profiles.ci]
test_timeout = "5m"

[profiles.dev]
test_timeout = "10m"
"#,
        )
        .unwrap();

        let (resolved, active) = cfg.resolve(Some("dev")).unwrap();
        assert_eq!(active.as_deref(), Some("dev"));
        assert_eq!(resolved.test_timeout.as_deref(), Some("10m"));
    }

    #[test]
    fn profile_can_supply_test_command_when_base_lacks_it() {
        let cfg = ConfigFile::parse_toml(
            r#"
[profiles.ci]
test_command = "python -m test"
"#,
        )
        .unwrap();

        // Base alone would fail cross-field validation (no test_command)…
        assert!(cfg.resolve(None).is_err());
        // …but the profile supplies it.
        let (resolved, _) = cfg.resolve(Some("ci")).unwrap();
        assert_eq!(resolved.test_command, "python -m test");
    }

    #[test]
    fn profile_per_layer_validation_catches_bad_test_order() {
        let err = ConfigFile::parse_toml(
            r#"
test_command = "echo"

[profiles.ci]
test_order = "bogus"
"#,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "Configuration error: profile 'ci': unknown test order 'bogus': \
             expected one of auto, discovery, alphabetical, failing-first, spread, \
             shuffle[:<seed>], slowest-first, fastest-first, frequent-failing-first"
        );
    }

    #[test]
    fn fields_from_profile_reports_overlaid_keys() {
        let cfg = ConfigFile::parse_toml(
            r#"
test_command = "echo"
filter_tags = "worker-0"

[profiles.ci]
test_timeout = "5m"
filter_tags = ""
"#,
        )
        .unwrap();
        let mut from: Vec<&str> = cfg.fields_from_profile(Some("ci")).into_iter().collect();
        from.sort();
        assert_eq!(from, vec!["filter_tags", "test_timeout"]);
    }

    #[test]
    fn fields_from_profile_empty_when_no_profile_active() {
        let cfg = ConfigFile::parse_toml(
            r#"
test_command = "echo"

[profiles.ci]
test_timeout = "5m"
"#,
        )
        .unwrap();
        let from: Vec<&str> = cfg.fields_from_profile(None).into_iter().collect();
        assert_eq!(from, Vec::<&str>::new());

        let from: Vec<&str> = cfg
            .fields_from_profile(Some("default"))
            .into_iter()
            .collect();
        assert_eq!(from, Vec::<&str>::new());
    }

    #[test]
    fn profile_names_are_deterministically_ordered() {
        let cfg = ConfigFile::parse_toml(
            r#"
test_command = "echo"

[profiles.zeta]
test_timeout = "1m"

[profiles.alpha]
test_timeout = "2m"
"#,
        )
        .unwrap();
        assert_eq!(cfg.profile_names(), vec!["alpha", "zeta"]);
    }

    #[test]
    fn flat_toml_still_loads_unchanged() {
        // Backwards compatibility: flat layout without any profiles.
        let cfg = ConfigFile::parse_toml(
            r#"
test_command = "cargo subunit $LISTOPT $IDOPTION"
test_id_option = "--load-list $IDFILE"
test_list_option = "--list"
"#,
        )
        .unwrap();
        assert!(cfg.profiles.is_empty());
        assert!(cfg.default_profile.is_none());

        let (resolved, active) = cfg.resolve(None).unwrap();
        assert!(active.is_none());
        assert_eq!(resolved.test_command, "cargo subunit $LISTOPT $IDOPTION");
    }
}
