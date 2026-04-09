//! Export test results in standard formats

use crate::commands::utils::{open_repository, resolve_run_id};
use crate::commands::Command;
use crate::error::Result;
use crate::repository::{TestRun, TestStatus};
use crate::ui::UI;
use std::fmt::Write as FmtWrite;

/// Output format for test result export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// JSON format.
    Json,
    /// JUnit XML format.
    #[cfg(feature = "junit")]
    Junit,
    /// TAP (Test Anything Protocol) format.
    Tap,
}

impl std::str::FromStr for ExportFormat {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "json" => Ok(ExportFormat::Json),
            #[cfg(feature = "junit")]
            "junit" => Ok(ExportFormat::Junit),
            #[cfg(not(feature = "junit"))]
            "junit" => Err("JUnit format requires the 'junit' feature".to_string()),
            "tap" => Ok(ExportFormat::Tap),
            _ => {
                #[cfg(feature = "junit")]
                let expected = "json, junit, tap";
                #[cfg(not(feature = "junit"))]
                let expected = "json, tap";
                Err(format!(
                    "unknown format '{}', expected one of: {}",
                    s, expected
                ))
            }
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
            #[cfg(feature = "junit")]
            ExportFormat::Junit => export_junit(&test_run)?,
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

#[cfg(feature = "junit")]
fn export_junit(test_run: &TestRun) -> Result<String> {
    use quick_junit::{NonSuccessKind, Report, TestCase, TestCaseStatus, TestSuite};

    let mut suite = TestSuite::new(format!("run-{}", test_run.id));
    suite.set_timestamp(test_run.timestamp);
    if let Some(duration) = test_run.total_duration() {
        suite.set_time(duration);
    }

    // Sort results by test ID for deterministic output
    let mut results: Vec<_> = test_run.results.values().collect();
    results.sort_by_key(|r| r.test_id.as_str());

    for result in results {
        let status = match result.status {
            TestStatus::Success => TestCaseStatus::success(),
            TestStatus::Failure | TestStatus::UnexpectedSuccess => {
                let mut s = TestCaseStatus::non_success(NonSuccessKind::Failure);
                if let Some(msg) = &result.message {
                    s.set_message(msg.clone());
                }
                if let Some(details) = &result.details {
                    s.set_description(details.clone());
                }
                s
            }
            TestStatus::Error => {
                let mut s = TestCaseStatus::non_success(NonSuccessKind::Error);
                if let Some(msg) = &result.message {
                    s.set_message(msg.clone());
                }
                if let Some(details) = &result.details {
                    s.set_description(details.clone());
                }
                s
            }
            TestStatus::Skip | TestStatus::ExpectedFailure => {
                let mut s = TestCaseStatus::skipped();
                if let Some(msg) = &result.message {
                    s.set_message(msg.clone());
                }
                s
            }
        };

        let test_id_str = result.test_id.as_str();

        // Split test ID into classname and name at the last '.' or '::'
        let (classname, name) = if let Some(pos) = test_id_str.rfind("::") {
            (&test_id_str[..pos], &test_id_str[pos + 2..])
        } else if let Some(pos) = test_id_str.rfind('.') {
            (&test_id_str[..pos], &test_id_str[pos + 1..])
        } else {
            ("", test_id_str)
        };

        let mut tc = TestCase::new(name, status);
        if !classname.is_empty() {
            tc.set_classname(classname);
        }
        if let Some(duration) = result.duration {
            tc.set_time(duration);
        }

        suite.add_test_case(tc);
    }

    let mut report = Report::new("inquest");
    report.add_test_suite(suite);

    report
        .to_string()
        .map_err(|e| format!("JUnit serialization error: {}", e).into())
}

fn export_tap(test_run: &TestRun) -> String {
    let mut out = String::new();

    // Sort results by test ID for deterministic output
    let mut results: Vec<_> = test_run.results.values().collect();
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
    use crate::repository::{RepositoryFactory, RunId, TestResult, TestRun};
    use std::time::Duration;
    use tempfile::TempDir;

    fn make_test_run() -> TestRun {
        let mut test_run = TestRun::new(RunId::new("0"));
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

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["id"], "0");

        let results = value["results"].as_object().unwrap();
        assert_eq!(results.len(), 4);

        let alpha = &results["tests.unit.test_alpha"];
        assert_eq!(alpha["status"], "Success");
        assert_eq!(alpha["duration"]["secs"], 0);
        assert_eq!(alpha["duration"]["nanos"], 100_000_000);

        let beta = &results["tests.unit.test_beta"];
        assert_eq!(beta["status"], "Failure");
        assert_eq!(beta["message"], "assertion failed");
        assert_eq!(beta["details"], "Traceback:\n  line 42\nAssertionError");

        let gamma = &results["tests.unit.test_gamma"];
        assert_eq!(gamma["status"], "Skip");
        assert_eq!(gamma["duration"], serde_json::Value::Null);

        let delta = &results["tests.unit.test_delta"];
        assert_eq!(delta["status"], "Error");
        assert_eq!(delta["message"], "timeout");
    }

    #[cfg(feature = "junit")]
    #[test]
    fn test_export_junit() {
        let test_run = make_test_run();
        let xml = export_junit(&test_run).unwrap();

        // Helper to convert XmlString to &str without ambiguous as_ref()
        fn xml_str(s: &quick_junit::XmlString) -> &str {
            s
        }
        fn xml_str_opt(s: &Option<quick_junit::XmlString>) -> Option<&str> {
            s.as_deref()
        }

        // Parse the XML back to verify structure
        let report = quick_junit::Report::deserialize_from_str(&xml).unwrap();
        assert_eq!(xml_str(&report.name), "inquest");
        assert_eq!(report.test_suites.len(), 1);

        let suite = &report.test_suites[0];
        assert_eq!(xml_str(&suite.name), "run-0");
        assert_eq!(suite.test_cases.len(), 4);

        // Tests are sorted by ID: test_alpha, test_beta, test_delta, test_gamma
        let alpha = &suite.test_cases[0];
        assert_eq!(xml_str(&alpha.name), "test_alpha");
        assert_eq!(xml_str_opt(&alpha.classname), Some("tests.unit"));
        assert_eq!(alpha.time, Some(Duration::from_millis(100)));
        assert!(matches!(
            alpha.status,
            quick_junit::TestCaseStatus::Success { .. }
        ));

        let beta = &suite.test_cases[1];
        assert_eq!(xml_str(&beta.name), "test_beta");
        assert_eq!(beta.time, Some(Duration::from_millis(200)));
        match &beta.status {
            quick_junit::TestCaseStatus::NonSuccess {
                kind,
                message,
                description,
                ..
            } => {
                assert_eq!(*kind, quick_junit::NonSuccessKind::Failure);
                assert_eq!(xml_str_opt(message), Some("assertion failed"));
                assert_eq!(
                    xml_str_opt(description),
                    Some("Traceback:\n  line 42\nAssertionError")
                );
            }
            other => panic!("expected NonSuccess, got {:?}", other),
        }

        let delta = &suite.test_cases[2];
        assert_eq!(xml_str(&delta.name), "test_delta");
        match &delta.status {
            quick_junit::TestCaseStatus::NonSuccess { kind, message, .. } => {
                assert_eq!(*kind, quick_junit::NonSuccessKind::Error);
                assert_eq!(xml_str_opt(message), Some("timeout"));
            }
            other => panic!("expected NonSuccess, got {:?}", other),
        }

        let gamma = &suite.test_cases[3];
        assert_eq!(xml_str(&gamma.name), "test_gamma");
        assert!(matches!(
            gamma.status,
            quick_junit::TestCaseStatus::Skipped { .. }
        ));
    }

    #[test]
    fn test_export_tap() {
        let test_run = make_test_run();
        let tap = export_tap(&test_run);

        assert_eq!(
            tap,
            "\
TAP version 13
1..4
ok 1 tests.unit.test_alpha
not ok 2 tests.unit.test_beta
  ---
  message: assertion failed
  severity: failure
  data: |
    Traceback:
      line 42
    AssertionError
  ...
not ok 3 tests.unit.test_delta
  ---
  message: timeout
  severity: error
  ...
ok 4 tests.unit.test_gamma # SKIP
"
        );
    }

    #[test]
    fn test_export_command_json() {
        let temp = TempDir::new().unwrap();

        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new(RunId::new("0"));
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult::success("test1"));
        repo.insert_test_run(test_run).unwrap();

        let mut ui = crate::ui::test_ui::TestUI::new();
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

    #[cfg(feature = "junit")]
    #[test]
    fn test_export_command_junit() {
        let temp = TempDir::new().unwrap();

        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new(RunId::new("0"));
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult::success("test1"));
        repo.insert_test_run(test_run).unwrap();

        let mut ui = crate::ui::test_ui::TestUI::new();
        let cmd = ExportCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            None,
            ExportFormat::Junit,
        );
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);

        let output = ui.output.join("\n");
        let report = quick_junit::Report::deserialize_from_str(&output).unwrap();
        assert_eq!(report.test_suites.len(), 1);
        assert_eq!(report.test_suites[0].test_cases.len(), 1);
        assert_eq!(&*report.test_suites[0].test_cases[0].name, "test1");
    }

    #[test]
    fn test_export_command_tap() {
        let temp = TempDir::new().unwrap();

        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new(RunId::new("0"));
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult::success("test1"));
        repo.insert_test_run(test_run).unwrap();

        let mut ui = crate::ui::test_ui::TestUI::new();
        let cmd = ExportCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            None,
            ExportFormat::Tap,
        );
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);

        let output = ui.output.join("\n");
        assert_eq!(output, "TAP version 13\n1..1\nok 1 test1\n");
    }

    #[test]
    fn test_export_format_from_str() {
        assert_eq!("json".parse::<ExportFormat>().unwrap(), ExportFormat::Json);
        #[cfg(feature = "junit")]
        assert_eq!(
            "junit".parse::<ExportFormat>().unwrap(),
            ExportFormat::Junit
        );
        assert_eq!("tap".parse::<ExportFormat>().unwrap(), ExportFormat::Tap);
        assert_eq!("JSON".parse::<ExportFormat>().unwrap(), ExportFormat::Json);
        assert!("csv".parse::<ExportFormat>().is_err());
    }

    #[cfg(feature = "junit")]
    #[test]
    fn test_junit_classname_splitting() {
        let mut test_run = TestRun::new(RunId::new("0"));
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();

        test_run.add_result(TestResult::success("tests.unit.test_foo"));
        test_run.add_result(TestResult::success("tests::unit::test_bar"));
        test_run.add_result(TestResult::success("simple_test"));

        let xml = export_junit(&test_run).unwrap();
        let report = quick_junit::Report::deserialize_from_str(&xml).unwrap();
        let cases = &report.test_suites[0].test_cases;

        fn xml_str(s: &quick_junit::XmlString) -> &str {
            s
        }
        fn xml_str_opt(s: &Option<quick_junit::XmlString>) -> Option<&str> {
            s.as_deref()
        }

        // Sorted by test ID: simple_test, tests.unit.test_foo, tests::unit::test_bar
        assert_eq!(xml_str(&cases[0].name), "simple_test");
        assert_eq!(xml_str_opt(&cases[0].classname), None);

        assert_eq!(xml_str(&cases[1].name), "test_foo");
        assert_eq!(xml_str_opt(&cases[1].classname), Some("tests.unit"));

        assert_eq!(xml_str(&cases[2].name), "test_bar");
        assert_eq!(xml_str_opt(&cases[2].classname), Some("tests::unit"));
    }
}
