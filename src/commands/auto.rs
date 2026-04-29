//! Auto-detect project type and generate an inquest.toml configuration file

use crate::commands::Command;
use crate::config::CONFIG_FILE_NAMES;
use crate::error::Result;
use crate::ui::UI;
use std::path::Path;

/// A detected project type with its recommended inquest configuration.
struct Detection {
    name: &'static str,
    test_command: &'static str,
    test_id_option: Option<&'static str>,
    test_list_option: Option<&'static str>,
}

/// Detect project type from files present in the directory.
fn detect_project(base: &Path) -> Vec<Detection> {
    let mut detections = Vec::new();

    // Rust/Cargo project
    if base.join("Cargo.toml").exists() {
        detections.push(Detection {
            name: "Cargo (Rust)",
            test_command: "cargo subunit $LISTOPT $IDOPTION",
            test_id_option: Some("--load-list $IDFILE"),
            test_list_option: Some("--list"),
        });
    }

    // Python project with pytest
    if has_pytest(base) {
        detections.push(Detection {
            name: "pytest (Python)",
            test_command: "pytest --subunit $IDOPTION",
            test_id_option: Some("--load-list $IDFILE"),
            test_list_option: None,
        });
    }

    // Python project with unittest / subunit
    if has_python_unittest(base) {
        detections.push(Detection {
            name: "unittest/subunit (Python)",
            test_command: "python3 -m subunit.run discover $IDOPTION $LISTOPT",
            test_id_option: Some("--load-list $IDFILE"),
            test_list_option: Some("--list"),
        });
    }

    // Go module — orchestrate `go test` via the `gotest-run` wrapper
    // (ships with python-subunit). The wrapper handles all three modes:
    // bare invocation runs the whole tree, `--list` enumerates tests as
    // subunit `exists` events (subtests aren't statically discoverable
    // and are absent), and `--id-file` fans out one `go test -json -run
    // <regex>` invocation per package so per-test selection works for
    // `--failing`, `--isolated`, and load-list mode.
    if has_go(base) {
        detections.push(Detection {
            name: "go test (Go)",
            test_command: "gotest-run $LISTOPT $IDOPTION",
            test_id_option: Some("--id-file $IDFILE"),
            test_list_option: Some("--list"),
        });
    }

    // Perl project using prove + tap2subunit (via prove-subunit wrapper)
    if has_perl(base) {
        detections.push(Detection {
            name: "prove (Perl)",
            test_command: "prove-subunit",
            test_id_option: None,
            test_list_option: None,
        });
    }

    // Node.js project with Vitest — Vitest ships a built-in TAP reporter
    // that writes to stdout, so a simple pipe through `tap2subunit`
    // (ships with python-subunit) produces a subunit v2 stream.
    if has_vitest(base) {
        detections.push(Detection {
            name: "vitest (Node.js)",
            test_command: "vitest run --reporter=tap | tap2subunit",
            test_id_option: None,
            test_list_option: None,
        });
    }

    // Node.js project with Jest — Jest has no built-in machine-readable
    // reporter, so we rely on the de-facto `jest-junit` package (which
    // the user must add as a devDependency) to emit `junit.xml`, then
    // post-process with `junitxml2subunit` (ships with python-subunit).
    // We use `;` rather than `&&` so the conversion still runs when
    // tests fail — inquest derives pass/fail from the subunit stream.
    if has_jest(base) {
        detections.push(Detection {
            name: "jest (Node.js)",
            test_command: "jest --ci --reporters=jest-junit; junitxml2subunit junit.xml",
            test_id_option: None,
            test_list_option: None,
        });
    }

    detections
}

/// Check if the project is a Go module.
fn has_go(base: &Path) -> bool {
    base.join("go.mod").exists()
}

/// Check if the project uses pytest.
fn has_pytest(base: &Path) -> bool {
    // Check for pytest config files
    for name in &["pytest.ini", "conftest.py"] {
        if base.join(name).exists() {
            return true;
        }
    }

    // Check for [tool.pytest] in pyproject.toml
    if let Ok(contents) = std::fs::read_to_string(base.join("pyproject.toml")) {
        if contents.contains("[tool.pytest") {
            return true;
        }
    }

    // Check for [pytest] in setup.cfg
    if let Ok(contents) = std::fs::read_to_string(base.join("setup.cfg")) {
        if contents.contains("[tool:pytest]") {
            return true;
        }
    }

    false
}

/// Check if the project uses Python unittest with subunit.
fn has_python_unittest(base: &Path) -> bool {
    // Look for setup.py, setup.cfg, or pyproject.toml as indicators of a Python project
    for name in &["setup.py", "setup.cfg", "pyproject.toml"] {
        if base.join(name).exists() {
            return true;
        }
    }

    false
}

/// Check if the project uses Vitest.
fn has_vitest(base: &Path) -> bool {
    for name in &[
        "vitest.config.js",
        "vitest.config.ts",
        "vitest.config.mjs",
        "vitest.config.cjs",
        "vitest.config.mts",
        "vitest.config.cts",
    ] {
        if base.join(name).exists() {
            return true;
        }
    }
    package_json_mentions(base, "vitest")
}

/// Check if the project uses Jest.
fn has_jest(base: &Path) -> bool {
    for name in &[
        "jest.config.js",
        "jest.config.ts",
        "jest.config.mjs",
        "jest.config.cjs",
        "jest.config.json",
    ] {
        if base.join(name).exists() {
            return true;
        }
    }
    package_json_mentions(base, "jest")
}

/// Look for `name` as a key under "dependencies"/"devDependencies" in
/// package.json, or as a top-level key (Jest accepts a `"jest"` block in
/// package.json as a config source).
fn package_json_mentions(base: &Path, name: &str) -> bool {
    let Ok(contents) = std::fs::read_to_string(base.join("package.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    if value.get(name).is_some() {
        return true;
    }
    for section in &["dependencies", "devDependencies", "peerDependencies"] {
        if value
            .get(section)
            .and_then(|s| s.as_object())
            .is_some_and(|m| m.contains_key(name))
        {
            return true;
        }
    }
    false
}

/// Check if the project is a Perl project with a test suite.
fn has_perl(base: &Path) -> bool {
    // Standard Perl build/metadata files
    for name in &["cpanfile", "Makefile.PL", "Build.PL", "dist.ini"] {
        if base.join(name).exists() {
            return true;
        }
    }

    // t/ directory with .t files is the conventional Perl test layout
    let t_dir = base.join("t");
    if t_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&t_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|e| e == "t") {
                    return true;
                }
            }
        }
    }

    false
}

/// Format a Detection as TOML content.
fn format_toml(detection: &Detection) -> String {
    let mut lines = Vec::new();
    lines.push(format!("test_command = {:?}", detection.test_command));
    if let Some(opt) = detection.test_id_option {
        lines.push(format!("test_id_option = {:?}", opt));
    }
    if let Some(opt) = detection.test_list_option {
        lines.push(format!("test_list_option = {:?}", opt));
    }
    lines.join("\n") + "\n"
}

/// Command to auto-detect project type and generate an inquest.toml configuration file.
pub struct AutoCommand {
    base_path: Option<String>,
}

impl AutoCommand {
    /// Creates a new auto command.
    pub fn new(base_path: Option<String>) -> Self {
        AutoCommand { base_path }
    }
}

impl Command for AutoCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = self
            .base_path
            .as_deref()
            .map_or_else(|| Path::new("."), Path::new);

        // Check if a config file already exists
        for name in CONFIG_FILE_NAMES {
            let path = base.join(name);
            if path.exists() {
                ui.error(&format!(
                    "Configuration file already exists: {}",
                    path.display()
                ))?;
                return Ok(1);
            }
        }

        let detections = detect_project(base);

        if detections.is_empty() {
            ui.error("Could not detect project type")?;
            ui.error(
                "Supported project types: Cargo (Rust), pytest (Python), unittest/subunit (Python), go test (Go), prove (Perl), vitest (Node.js), jest (Node.js)",
            )?;
            return Ok(1);
        }

        // Use the first (highest priority) detection
        let detection = &detections[0];
        let toml_content = format_toml(detection);

        let config_path = base.join("inquest.toml");
        std::fs::write(&config_path, &toml_content).map_err(|e| {
            crate::error::Error::Config(format!("Failed to write {}: {}", config_path.display(), e))
        })?;

        ui.output(&format!(
            "Detected {} project, wrote {}",
            detection.name,
            config_path.display()
        ))?;

        Ok(0)
    }

    fn name(&self) -> &str {
        "auto"
    }

    fn help(&self) -> &str {
        "Auto-detect project type and generate inquest.toml"
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
    fn test_auto_detect_cargo() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected Cargo (Rust) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );

        let content = std::fs::read_to_string(temp.path().join("inquest.toml")).unwrap();
        assert_eq!(
            content,
            "test_command = \"cargo subunit $LISTOPT $IDOPTION\"\n\
             test_id_option = \"--load-list $IDFILE\"\n\
             test_list_option = \"--list\"\n"
        );
    }

    #[test]
    fn test_auto_detect_pytest() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("pytest.ini"), "").unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected pytest (Python) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );

        let content = std::fs::read_to_string(temp.path().join("inquest.toml")).unwrap();
        assert_eq!(
            content,
            "test_command = \"pytest --subunit $IDOPTION\"\n\
             test_id_option = \"--load-list $IDFILE\"\n"
        );
    }

    #[test]
    fn test_auto_detect_pytest_from_pyproject() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("pyproject.toml"),
            "[tool.pytest.ini_options]\naddopts = \"-v\"\n",
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected pytest (Python) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );
    }

    #[test]
    fn test_auto_detect_python_unittest() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("setup.py"), "").unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected unittest/subunit (Python) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );

        let content = std::fs::read_to_string(temp.path().join("inquest.toml")).unwrap();
        assert_eq!(
            content,
            "test_command = \"python3 -m subunit.run discover $IDOPTION $LISTOPT\"\n\
             test_id_option = \"--load-list $IDFILE\"\n\
             test_list_option = \"--list\"\n"
        );
    }

    #[test]
    fn test_auto_no_project_detected() {
        let temp = TempDir::new().unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 1);
        assert_eq!(
            ui.errors,
            vec![
                "Could not detect project type",
                "Supported project types: Cargo (Rust), pytest (Python), unittest/subunit (Python), go test (Go), prove (Perl), vitest (Node.js), jest (Node.js)",
            ]
        );
    }

    #[test]
    fn test_auto_detect_go() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("go.mod"),
            "module example.com/foo\n\ngo 1.22\n",
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected go test (Go) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );

        let content = std::fs::read_to_string(temp.path().join("inquest.toml")).unwrap();
        assert_eq!(
            content,
            "test_command = \"gotest-run $LISTOPT $IDOPTION\"\n\
             test_id_option = \"--id-file $IDFILE\"\n\
             test_list_option = \"--list\"\n"
        );
    }

    #[test]
    fn test_auto_detect_perl_cpanfile() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("cpanfile"), "requires 'Test::More';\n").unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected prove (Perl) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );

        let content = std::fs::read_to_string(temp.path().join("inquest.toml")).unwrap();
        assert_eq!(content, "test_command = \"prove-subunit\"\n");
    }

    #[test]
    fn test_auto_detect_perl_t_directory() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir(temp.path().join("t")).unwrap();
        std::fs::write(
            temp.path().join("t").join("basic.t"),
            "use Test::More tests => 1;\nok(1);\n",
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected prove (Perl) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );
    }

    #[test]
    fn test_auto_detect_perl_empty_t_directory() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir(temp.path().join("t")).unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 1);
    }

    #[test]
    fn test_auto_config_already_exists() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("inquest.toml"), "test_command = \"foo\"\n").unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 1);
        assert_eq!(
            ui.errors,
            vec![format!(
                "Configuration file already exists: {}",
                temp.path().join("inquest.toml").display()
            )]
        );
    }

    #[test]
    fn test_auto_pytest_priority_over_unittest() {
        let temp = TempDir::new().unwrap();
        // Both pytest and generic Python indicators present
        std::fs::write(temp.path().join("conftest.py"), "").unwrap();
        std::fs::write(temp.path().join("setup.py"), "").unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        // pytest should win over unittest
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected pytest (Python) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );
    }

    #[test]
    fn test_auto_detect_vitest_config() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("vitest.config.ts"), "export default {}\n").unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected vitest (Node.js) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );

        let content = std::fs::read_to_string(temp.path().join("inquest.toml")).unwrap();
        assert_eq!(
            content,
            "test_command = \"vitest run --reporter=tap | tap2subunit\"\n"
        );
    }

    #[test]
    fn test_auto_detect_vitest_devdependency() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"name":"foo","devDependencies":{"vitest":"^1.0.0"}}"#,
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected vitest (Node.js) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );
    }

    #[test]
    fn test_auto_detect_jest_config() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("jest.config.js"), "module.exports = {};\n").unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected jest (Node.js) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );

        let content = std::fs::read_to_string(temp.path().join("inquest.toml")).unwrap();
        assert_eq!(
            content,
            "test_command = \"jest --ci --reporters=jest-junit; junitxml2subunit junit.xml\"\n"
        );
    }

    #[test]
    fn test_auto_detect_jest_package_json_block() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"name":"foo","jest":{"testEnvironment":"node"}}"#,
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected jest (Node.js) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );
    }

    #[test]
    fn test_auto_detect_jest_devdependency() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"name":"foo","devDependencies":{"jest":"^29.0.0"}}"#,
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected jest (Node.js) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );
    }

    #[test]
    fn test_auto_vitest_priority_over_jest() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"devDependencies":{"vitest":"^1","jest":"^29"}}"#,
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected vitest (Node.js) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );
    }

    #[test]
    fn test_auto_node_no_test_runner_not_detected() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"name":"foo","dependencies":{"lodash":"^4"}}"#,
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 1);
    }

    #[test]
    fn test_auto_package_json_invalid_json_no_panic() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("package.json"), "not json {").unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        // Falls through to "no project detected" since package.json is unparseable.
        assert_eq!(result, 1);
    }

    #[test]
    fn test_format_toml_all_fields() {
        let detection = Detection {
            name: "test",
            test_command: "test-cmd $LISTOPT $IDOPTION",
            test_id_option: Some("--load-list $IDFILE"),
            test_list_option: Some("--list"),
        };
        assert_eq!(
            format_toml(&detection),
            "test_command = \"test-cmd $LISTOPT $IDOPTION\"\n\
             test_id_option = \"--load-list $IDFILE\"\n\
             test_list_option = \"--list\"\n"
        );
    }

    #[test]
    fn test_format_toml_minimal() {
        let detection = Detection {
            name: "test",
            test_command: "test-cmd",
            test_id_option: None,
            test_list_option: None,
        };
        assert_eq!(format_toml(&detection), "test_command = \"test-cmd\"\n");
    }
}
