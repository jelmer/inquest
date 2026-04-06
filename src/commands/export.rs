//! Export test results in standard formats

use crate::commands::utils::{open_repository, resolve_run_id};
use crate::commands::Command;
use crate::error::Result;
use crate::repository::{TestResult, TestRun, TestStatus};
use crate::ui::UI;
use std::fmt::Write as FmtWrite;

/// Output format for test result export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// JSON format.
    Json,
    /// JUnit XML format.
    Junit,
    /// TAP (Test Anything Protocol) format.
    Tap,
}

impl std::str::FromStr for ExportFormat {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "json" => Ok(ExportFormat::Json),
            "junit" => Ok(ExportFormat::Junit),
            "tap" => Ok(ExportFormat::Tap),
            _ => Err(format!(
                "unknown format '{}', expected one of: json, junit, tap",
                s
            )),
        }
    }
}

/// Command to export test results in standard formats.
pub struct ExportCommand {
    base_path: Option<String>,
    run_id: Option<String>,
    format: ExportFormat,
}

impl ExportCommand {
    /// Creates a new export command.
    pub fn new(base_path: Option<String>, run_id: Option<String>, format: ExportFormat) -> Self {
        ExportCommand {
            base_path,
            run_id,
            format,
        }
    }
}

impl Command for ExportCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let repo = open_repository(self.base_path.as_deref())?;
        let run_id = resolve_run_id(&*repo, self.run_id.as_deref())?;
        let test_run = repo.get_test_run(&run_id)?;

        let output = match self.format {
            ExportFormat::Json => export_json(&test_run)?,
            ExportFormat::Junit => export_junit(&test_run),
            ExportFormat::Tap => export_tap(&test_run),
        };

        ui.output(&output)?;
        Ok(0)
    }

    fn name(&self) -> &str {
        "export"
    }

    fn help(&self) -> &str {
        "Export test results in standard formats (json, junit, tap)"
    }
}

fn export_json(test_run: &TestRun) -> Result<String> {
    serde_json::to_string_pretty(test_run).map_err(|e| e.to_string().into())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn export_junit(test_run: &TestRun) -> String {
    let mut out = String::new();

    let total = test_run.total_tests();
    let failures = test_run
        .results
        .values()
        .filter(|r| r.status == TestStatus::Failure)
        .count();
    let errors = test_run
        .results
        .values()
        .filter(|r| r.status == TestStatus::Error || r.status == TestStatus::UnexpectedSuccess)
        .count();
    let skipped = test_run
        .results
        .values()
        .filter(|r| r.status == TestStatus::Skip || r.status == TestStatus::ExpectedFailure)
        .count();
    let time_secs = test_run
        .total_duration()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    writeln!(out, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>").unwrap();
    writeln!(
        out,
        "<testsuite name=\"run-{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\" timestamp=\"{}\">",
        xml_escape(&test_run.id),
        total,
        failures,
        errors,
        skipped,
        time_secs,
        test_run.timestamp.format("%Y-%m-%dT%H:%M:%S"),
    )
    .unwrap();

    // Sort results by test ID for deterministic output
    let mut results: Vec<&TestResult> = test_run.results.values().collect();
    results.sort_by_key(|r| r.test_id.as_str());

    for result in results {
        let time = result
            .duration
            .map(|d| format!("{:.3}", d.as_secs_f64()))
            .unwrap_or_default();

        // Split test ID into classname and name at the last '.' or '::'
        let test_id_str = result.test_id.as_str();
        let (classname, name) = if let Some(pos) = test_id_str.rfind("::") {
            (&test_id_str[..pos], &test_id_str[pos + 2..])
        } else if let Some(pos) = test_id_str.rfind('.') {
            (&test_id_str[..pos], &test_id_str[pos + 1..])
        } else {
            ("", test_id_str)
        };

        write!(
            out,
            "  <testcase classname=\"{}\" name=\"{}\"",
            xml_escape(classname),
            xml_escape(name),
        )
        .unwrap();
        if !time.is_empty() {
            write!(out, " time=\"{}\"", time).unwrap();
        }

        match result.status {
            TestStatus::Success => {
                writeln!(out, " />").unwrap();
            }
            TestStatus::Failure | TestStatus::UnexpectedSuccess => {
                writeln!(out, ">").unwrap();
                let msg = result.message.as_deref().unwrap_or("");
                write!(out, "    <failure message=\"{}\"", xml_escape(msg)).unwrap();
                if let Some(details) = &result.details {
                    writeln!(out, ">{}</failure>", xml_escape(details)).unwrap();
                } else {
                    writeln!(out, " />").unwrap();
                }
                writeln!(out, "  </testcase>").unwrap();
            }
            TestStatus::Error => {
                writeln!(out, ">").unwrap();
                let msg = result.message.as_deref().unwrap_or("");
                write!(out, "    <error message=\"{}\"", xml_escape(msg)).unwrap();
                if let Some(details) = &result.details {
                    writeln!(out, ">{}</error>", xml_escape(details)).unwrap();
                } else {
                    writeln!(out, " />").unwrap();
                }
                writeln!(out, "  </testcase>").unwrap();
            }
            TestStatus::Skip | TestStatus::ExpectedFailure => {
                writeln!(out, ">").unwrap();
                let msg = result.message.as_deref().unwrap_or("skipped");
                writeln!(out, "    <skipped message=\"{}\" />", xml_escape(msg)).unwrap();
                writeln!(out, "  </testcase>").unwrap();
            }
        }
    }

    writeln!(out, "</testsuite>").unwrap();
    out
}

fn export_tap(test_run: &TestRun) -> String {
    let mut out = String::new();

    // Sort results by test ID for deterministic output
    let mut results: Vec<&TestResult> = test_run.results.values().collect();
    results.sort_by_key(|r| r.test_id.as_str());

    writeln!(out, "TAP version 13").unwrap();
    writeln!(out, "1..{}", results.len()).unwrap();

    for (i, result) in results.iter().enumerate() {
        let num = i + 1;
        let test_id = result.test_id.as_str();

        match result.status {
            TestStatus::Success => {
                writeln!(out, "ok {} {}", num, test_id).unwrap();
            }
            TestStatus::Failure | TestStatus::Error | TestStatus::UnexpectedSuccess => {
                writeln!(out, "not ok {} {}", num, test_id).unwrap();
                // Add diagnostics as YAML block
                if result.message.is_some() || result.details.is_some() {
                    writeln!(out, "  ---").unwrap();
                    if let Some(msg) = &result.message {
                        writeln!(out, "  message: {}", msg).unwrap();
                    }
                    writeln!(out, "  severity: {}", result.status).unwrap();
                    if let Some(details) = &result.details {
                        writeln!(out, "  data: |").unwrap();
                        for line in details.lines() {
                            writeln!(out, "    {}", line).unwrap();
                        }
                    }
                    writeln!(out, "  ...").unwrap();
                }
            }
            TestStatus::Skip | TestStatus::ExpectedFailure => {
                let reason = result.message.as_deref().unwrap_or("");
                if reason.is_empty() {
                    writeln!(out, "ok {} {} # SKIP", num, test_id).unwrap();
                } else {
                    writeln!(out, "ok {} {} # SKIP {}", num, test_id, reason).unwrap();
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, TestResult, TestRun};
    use crate::ui::test_ui::TestUI;
    use std::time::Duration;
    use tempfile::TempDir;

    fn make_test_run() -> TestRun {
        let mut test_run = TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();

        test_run.add_result(
            TestResult::success("tests.unit.test_alpha").with_duration(Duration::from_millis(100)),
        );
        test_run.add_result(
            TestResult::failure("tests.unit.test_beta", "assertion failed")
                .with_duration(Duration::from_millis(200))
                .with_details("Traceback:\n  line 42\nAssertionError"),
        );
        test_run.add_result(TestResult::skip("tests.unit.test_gamma"));
        test_run.add_result(
            TestResult::error("tests.unit.test_delta", "timeout")
                .with_duration(Duration::from_millis(5000)),
        );

        test_run
    }

    #[test]
    fn test_export_json() {
        let test_run = make_test_run();
        let json = export_json(&test_run).unwrap();

        // Verify it's valid JSON by parsing
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["id"], "0");

        let results = value["results"].as_object().unwrap();
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn test_export_junit() {
        let test_run = make_test_run();
        let xml = export_junit(&test_run);

        assert!(xml.starts_with("<?xml version=\"1.0\""));
        assert!(xml.contains("<testsuite"));
        assert!(xml.contains("tests=\"4\""));
        assert!(xml.contains("failures=\"1\""));
        assert!(xml.contains("errors=\"1\""));
        assert!(xml.contains("skipped=\"1\""));
        assert!(xml.contains("classname=\"tests.unit\""));
        assert!(xml.contains("name=\"test_alpha\""));
        assert!(xml.contains("<failure message=\"assertion failed\""));
        assert!(xml.contains("<error message=\"timeout\""));
        assert!(xml.contains("<skipped"));
        assert!(xml.contains("</testsuite>"));
    }

    #[test]
    fn test_export_tap() {
        let test_run = make_test_run();
        let tap = export_tap(&test_run);

        assert!(tap.starts_with("TAP version 13\n"));
        assert!(tap.contains("1..4\n"));
        assert!(tap.contains("ok 1 tests.unit.test_alpha\n"));
        assert!(tap.contains("not ok 2 tests.unit.test_beta\n"));
        assert!(tap.contains("not ok 3 tests.unit.test_delta\n"));
        assert!(tap.contains("# SKIP"));
    }

    #[test]
    fn test_export_command_json() {
        let temp = TempDir::new().unwrap();

        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult::success("test1"));
        repo.insert_test_run(test_run).unwrap();

        let mut ui = TestUI::new();
        let cmd = ExportCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            None,
            ExportFormat::Json,
        );
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);

        let output = ui.output.join("\n");
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["id"], "0");
    }

    #[test]
    fn test_export_command_junit() {
        let temp = TempDir::new().unwrap();

        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult::success("test1"));
        repo.insert_test_run(test_run).unwrap();

        let mut ui = TestUI::new();
        let cmd = ExportCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            None,
            ExportFormat::Junit,
        );
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);

        let output = ui.output.join("\n");
        assert!(output.contains("<testsuite"));
        assert!(output.contains("tests=\"1\""));
    }

    #[test]
    fn test_export_command_tap() {
        let temp = TempDir::new().unwrap();

        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult::success("test1"));
        repo.insert_test_run(test_run).unwrap();

        let mut ui = TestUI::new();
        let cmd = ExportCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            None,
            ExportFormat::Tap,
        );
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);

        let output = ui.output.join("\n");
        assert!(output.contains("TAP version 13"));
        assert!(output.contains("1..1"));
        assert!(output.contains("ok 1 test1"));
    }

    #[test]
    fn test_export_format_from_str() {
        assert_eq!("json".parse::<ExportFormat>().unwrap(), ExportFormat::Json);
        assert_eq!(
            "junit".parse::<ExportFormat>().unwrap(),
            ExportFormat::Junit
        );
        assert_eq!("tap".parse::<ExportFormat>().unwrap(), ExportFormat::Tap);
        assert_eq!("JSON".parse::<ExportFormat>().unwrap(), ExportFormat::Json);
        assert!("csv".parse::<ExportFormat>().is_err());
    }

    #[test]
    fn test_junit_xml_escaping() {
        let mut test_run = TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(
            TestResult::failure("test<special>&chars", "msg with \"quotes\" & <brackets>")
                .with_details("details with <xml> & \"quotes\""),
        );

        let xml = export_junit(&test_run);
        assert!(xml.contains("&lt;special&gt;&amp;chars"));
        assert!(xml.contains("&quot;quotes&quot;"));
    }

    #[test]
    fn test_junit_classname_splitting() {
        let mut test_run = TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();

        // Python-style dotted path
        test_run.add_result(TestResult::success("tests.unit.test_foo"));
        // Rust-style :: path
        test_run.add_result(TestResult::success("tests::unit::test_bar"));
        // No separator
        test_run.add_result(TestResult::success("simple_test"));

        let xml = export_junit(&test_run);
        assert!(xml.contains("classname=\"tests.unit\" name=\"test_foo\""));
        assert!(xml.contains("classname=\"tests::unit\" name=\"test_bar\""));
        assert!(xml.contains("classname=\"\" name=\"simple_test\""));
    }
}
