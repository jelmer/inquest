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

    // Maven / Gradle — drive the build tool through `jvmtest-subunit`
    // (ships with python-subunit). The wrapper auto-detects the
    // build tool from the current directory, spawns it, watches its
    // reports directory live, and translates inquest's $LISTOPT /
    // $IDOPTION into the build tool's selection vocabulary
    // (`-Dtest=` for Maven, `--tests` for Gradle). Listing is done
    // by walking src/test/java and src/test/kotlin for conventionally-
    // named test classes; methods created at runtime
    // (`@ParameterizedTest`, `@TestFactory`) aren't statically
    // discoverable and are absent from listings, but executing them
    // by ID works.
    if has_maven(base) {
        detections.push(Detection {
            name: "Maven (JVM)",
            test_command: "jvmtest-subunit $LISTOPT $IDOPTION",
            test_id_option: Some("--id-file $IDFILE"),
            test_list_option: Some("--list"),
        });
    }

    if has_gradle(base) {
        detections.push(Detection {
            name: "Gradle (JVM)",
            test_command: "jvmtest-subunit $LISTOPT $IDOPTION",
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

    detections
}

/// Check if the project is a Go module.
fn has_go(base: &Path) -> bool {
    base.join("go.mod").exists()
}

/// Check if the project is a Maven project.
fn has_maven(base: &Path) -> bool {
    base.join("pom.xml").exists()
}

/// Check if the project is a Gradle project. Both Groovy (`build.gradle`)
/// and Kotlin (`build.gradle.kts`) DSLs are recognised, plus the
/// `settings.gradle*` markers used by multi-module builds whose root
/// directory has no top-level `build.gradle`.
fn has_gradle(base: &Path) -> bool {
    for name in &[
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
    ] {
        if base.join(name).exists() {
            return true;
        }
    }
    false
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
            .map(Path::new)
            .unwrap_or_else(|| Path::new("."));

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
                "Supported project types: Cargo (Rust), pytest (Python), unittest/subunit (Python), go test (Go), Maven (JVM), Gradle (JVM), prove (Perl)",
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
                "Supported project types: Cargo (Rust), pytest (Python), unittest/subunit (Python), go test (Go), Maven (JVM), Gradle (JVM), prove (Perl)",
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
    fn test_auto_detect_maven() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("pom.xml"),
            "<project><groupId>x</groupId></project>\n",
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected Maven (JVM) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );

        let content = std::fs::read_to_string(temp.path().join("inquest.toml")).unwrap();
        assert_eq!(
            content,
            "test_command = \"jvmtest-subunit $LISTOPT $IDOPTION\"\n\
             test_id_option = \"--id-file $IDFILE\"\n\
             test_list_option = \"--list\"\n"
        );
    }

    #[test]
    fn test_auto_detect_gradle_groovy() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("build.gradle"), "apply plugin: 'java'\n").unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(
            ui.output,
            vec![format!(
                "Detected Gradle (JVM) project, wrote {}",
                temp.path().join("inquest.toml").display()
            )]
        );

        let content = std::fs::read_to_string(temp.path().join("inquest.toml")).unwrap();
        assert_eq!(
            content,
            "test_command = \"jvmtest-subunit $LISTOPT $IDOPTION\"\n\
             test_id_option = \"--id-file $IDFILE\"\n\
             test_list_option = \"--list\"\n"
        );
    }

    #[test]
    fn test_auto_detect_gradle_kotlin() {
        // The Kotlin DSL marker (build.gradle.kts) on its own should
        // also trigger Gradle detection.
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("build.gradle.kts"), "plugins { java }\n").unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert!(ui.output[0].contains("Detected Gradle (JVM)"));
    }

    #[test]
    fn test_auto_detect_gradle_settings_only() {
        // Multi-module Gradle builds often have only `settings.gradle`
        // at the root (the per-module `build.gradle` is one level down).
        // Detect from the settings file alone.
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("settings.gradle"),
            "rootProject.name = 'demo'\n",
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = AutoCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert!(ui.output[0].contains("Detected Gradle (JVM)"));
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
