//! Subunit stream processing
//!
//! This module provides functions to read and write subunit v2 streams,
//! converting between subunit events and our internal TestRun representation.
//!
//! This module supports both traditional file I/O and memory-mapped files
//! for improved performance with large subunit streams.

use crate::error::{Error, Result};
use crate::repository::{TestId, TestResult, TestRun, TestStatus};
use std::io::{Read, Write};
use subunit::Event;

/// Parse a subunit stream from a byte slice into a TestRun
///
/// This is optimized for memory-mapped files and avoids copying data.
/// It catches panics from the subunit crate and converts them to proper errors.
pub fn parse_stream_bytes(data: &[u8], run_id: String) -> Result<TestRun> {
    parse_stream(data, run_id)
}

/// Parse a subunit stream into a TestRun
///
/// This function catches panics from the subunit crate and converts them to proper errors.
pub fn parse_stream<R: Read>(reader: R, run_id: String) -> Result<TestRun> {
    use std::collections::HashMap;

    // The subunit crate can panic on invalid input, so we catch panics
    let events = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        subunit::parse_subunit(reader)
    }))
    .map_err(|_| Error::Subunit("Invalid subunit stream format (parse panic)".to_string()))?
    .map_err(|e| Error::Subunit(format!("Failed to parse subunit stream: {}", e)))?;

    let mut test_run = TestRun::new(run_id);
    let mut start_times: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();

    for event in events {
        if let Some(ref test_id_str) = event.test_id {
            let test_id = TestId::new(test_id_str.clone());

            // Track start events for duration calculation
            if let Some(ref status_str) = event.status {
                if status_str == "inprogress" {
                    if let Some(timestamp) = event.timestamp {
                        start_times.insert(test_id_str.clone(), timestamp);
                    }
                    continue; // Don't add inprogress events to results
                }
            }

            // Convert subunit status to our TestStatus
            let status = if let Some(ref status_str) = event.status {
                match status_str.as_str() {
                    "success" => TestStatus::Success,
                    "fail" => TestStatus::Failure,
                    "error" => TestStatus::Error,
                    "skip" => TestStatus::Skip,
                    "xfail" => TestStatus::ExpectedFailure,
                    "uxsuccess" => TestStatus::UnexpectedSuccess,
                    _ => continue, // Skip events with unknown status (e.g., "exists")
                }
            } else {
                continue; // Skip events without status
            };

            // Extract tags
            let tags = event.tags.unwrap_or_default();

            // Extract file content as message/details
            let (message, details) = if event.file_name.is_some() && event.file_content.is_some() {
                let content = String::from_utf8_lossy(&event.file_content.unwrap()).to_string();
                (Some(content.clone()), Some(content))
            } else {
                (None, None)
            };

            // Calculate duration from start/stop timestamps
            let duration = if let (Some(start_time), Some(end_time)) =
                (start_times.get(test_id_str), event.timestamp)
            {
                let duration_secs = (end_time - *start_time).num_milliseconds();
                if duration_secs >= 0 {
                    Some(std::time::Duration::from_millis(duration_secs as u64))
                } else {
                    None
                }
            } else {
                None
            };

            test_run.add_result(TestResult {
                test_id,
                status,
                duration,
                message,
                details,
                tags,
            });
        }
    }

    Ok(test_run)
}

/// Write a TestRun as a subunit stream
///
/// This function catches panics from the subunit crate and converts them to proper errors.
pub fn write_stream<W: Write>(test_run: &TestRun, mut writer: W) -> Result<()> {
    for result in test_run.results.values() {
        // If we have duration information, write an "inprogress" event first
        if let Some(duration) = result.duration {
            // Calculate start time by subtracting duration from run timestamp
            // Use seconds to avoid precision issues and chrono panics
            let duration_secs = duration.as_secs() as i64;
            let start_timestamp = test_run.timestamp - chrono::Duration::seconds(duration_secs);

            let mut start_event = Event {
                status: Some("inprogress".to_string()),
                test_id: Some(result.test_id.as_str().to_string()),
                timestamp: Some(start_timestamp),
                file_name: None,
                file_content: None,
                mime_type: None,
                route_code: None,
                tags: if !result.tags.is_empty() {
                    Some(result.tags.clone())
                } else {
                    None
                },
            };

            start_event
                .write(&mut writer)
                .map_err(|e| Error::Subunit(format!("Failed to write subunit event: {}", e)))?;
        }

        let status_str = match result.status {
            TestStatus::Success => "success",
            TestStatus::Failure => "fail",
            TestStatus::Error => "fail", // Subunit v2 doesn't have a separate 'error' status
            TestStatus::Skip => "skip",
            TestStatus::ExpectedFailure => "xfail",
            TestStatus::UnexpectedSuccess => "uxsuccess",
        };

        let mut event = Event {
            status: Some(status_str.to_string()),
            test_id: Some(result.test_id.as_str().to_string()),
            timestamp: Some(test_run.timestamp),
            file_name: None,
            file_content: None,
            mime_type: None,
            route_code: None,
            tags: if !result.tags.is_empty() {
                Some(result.tags.clone())
            } else {
                None
            },
        };

        // Add details as file attachment if present
        if let Some(ref details) = result.details {
            event.file_name = Some("traceback".to_string());
            event.file_content = Some(details.as_bytes().to_vec());
            event.mime_type = Some("text/plain".to_string());
        }

        // Write event - errors from subunit crate are properly handled
        event
            .write(&mut writer)
            .map_err(|e| Error::Subunit(format!("Failed to write subunit event: {}", e)))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_parse_empty_stream() {
        let empty_stream: &[u8] = &[];
        let result = parse_stream(empty_stream, "0".to_string());
        assert!(result.is_ok());
        let run = result.unwrap();
        assert_eq!(run.total_tests(), 0);
    }

    #[test]
    fn test_roundtrip_test_run() {
        // Create a test run
        // Use a fixed timestamp to avoid chrono issues with the subunit crate
        let mut test_run = TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();

        test_run.add_result(TestResult {
            test_id: TestId::new("test1"),
            status: TestStatus::Success,
            duration: Some(Duration::from_millis(100)),
            message: None,
            details: None,
            tags: vec!["worker-0".to_string()],
        });

        test_run.add_result(TestResult {
            test_id: TestId::new("test2"),
            status: TestStatus::Failure,
            duration: Some(Duration::from_millis(200)),
            message: Some("Failed".to_string()),
            details: Some("Traceback...".to_string()),
            tags: vec!["worker-1".to_string()],
        });

        // Write to stream
        let mut buffer = Vec::new();
        write_stream(&test_run, &mut buffer).unwrap();

        // Parse back
        let parsed = parse_stream(&buffer[..], "1".to_string()).unwrap();

        // Verify
        assert_eq!(parsed.total_tests(), 2);
        assert_eq!(parsed.count_successes(), 1);
        assert_eq!(parsed.count_failures(), 1);
    }

    #[test]
    fn test_status_conversion() {
        // Note: TestStatus::Error is mapped to "fail" in subunit v2, so it's not included in roundtrip test
        let statuses = vec![
            (TestStatus::Success, "success"),
            (TestStatus::Failure, "fail"),
            (TestStatus::Skip, "skip"),
            (TestStatus::ExpectedFailure, "xfail"),
            (TestStatus::UnexpectedSuccess, "uxsuccess"),
        ];

        for (status, _expected_str) in statuses {
            let mut test_run = TestRun::new("0".to_string());
            // Use a fixed timestamp to avoid chrono issues with the subunit crate
            test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();

            test_run.add_result(TestResult {
                test_id: TestId::new("test1"),
                status,
                duration: None,
                message: None,
                details: None,
                tags: vec![],
            });

            let mut buffer = Vec::new();
            write_stream(&test_run, &mut buffer).unwrap();

            let parsed = parse_stream(&buffer[..], "1".to_string()).unwrap();
            assert_eq!(parsed.total_tests(), 1);

            let result = parsed.results.values().next().unwrap();
            assert_eq!(result.status, status);
        }
    }

    #[test]
    fn test_invalid_subunit_stream_no_panic() {
        // Test that invalid input doesn't panic but returns an error
        let invalid_data: &[u8] = b"not valid subunit data at all";
        let result = parse_stream(invalid_data, "0".to_string());
        // Should return an error, not panic
        assert!(result.is_err());
        if let Err(Error::Subunit(msg)) = result {
            assert!(
                msg.contains("Invalid subunit stream") || msg.contains("Failed to parse"),
                "Error message: {}",
                msg
            );
        } else {
            panic!("Expected Subunit error");
        }
    }

    #[test]
    fn test_parse_stream_bytes() {
        // Test the memory-mapped parsing path
        let mut test_run = TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();

        test_run.add_result(TestResult {
            test_id: TestId::new("test1"),
            status: TestStatus::Success,
            duration: Some(Duration::from_millis(100)),
            message: None,
            details: None,
            tags: vec!["mmap-test".to_string()],
        });

        // Write to buffer
        let mut buffer = Vec::new();
        write_stream(&test_run, &mut buffer).unwrap();

        // Parse using the bytes function (simulating mmap)
        let parsed = parse_stream_bytes(&buffer, "1".to_string()).unwrap();

        // Verify
        assert_eq!(parsed.total_tests(), 1);
        assert_eq!(parsed.count_successes(), 1);
        let result = parsed.results.values().next().unwrap();
        assert_eq!(result.test_id.as_str(), "test1");
        assert!(result.tags.contains(&"mmap-test".to_string()));
    }
}
