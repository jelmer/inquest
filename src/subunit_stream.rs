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
use subunit::io::sync::iter_stream;
use subunit::serialize::Serializable;
use subunit::types::event::Event;
use subunit::types::stream::ScannedItem;
use subunit::types::teststatus::TestStatus as SubunitTestStatus;

/// Progress event status for test execution
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressStatus {
    /// Test is starting
    InProgress,
    /// Test passed
    Success,
    /// Test failed
    Failed,
    /// Test was skipped
    Skipped,
    /// Test failed as expected
    ExpectedFailure,
    /// Test passed unexpectedly
    UnexpectedSuccess,
}

impl ProgressStatus {
    /// Get the status indicator character for display
    pub fn indicator(&self) -> &'static str {
        match self {
            ProgressStatus::InProgress => "",
            ProgressStatus::Success => "✓",
            ProgressStatus::Failed => "✗",
            ProgressStatus::Skipped => "⊘",
            ProgressStatus::ExpectedFailure => "✓",
            ProgressStatus::UnexpectedSuccess => "✗",
        }
    }
}

/// Parse a subunit stream from a byte slice into a TestRun
///
/// This is optimized for memory-mapped files and avoids copying data.
pub fn parse_stream_bytes(data: &[u8], run_id: String) -> Result<TestRun> {
    parse_stream(data, run_id)
}

/// Parse a subunit stream into a TestRun with progress callback
///
/// The callback is called with (test_id, status) for each test event.
/// If the stream is incomplete or interrupted, returns partial results collected before the error.
/// Returns an error only for invalid timestamps in otherwise valid events.
pub fn parse_stream_with_progress<R: Read, F>(
    reader: R,
    run_id: String,
    mut progress_callback: F,
) -> Result<TestRun>
where
    F: FnMut(&str, ProgressStatus),
{
    use std::collections::HashMap;

    let mut test_run = TestRun::new(run_id);
    let mut start_times: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();

    // Iterate over the subunit stream
    for item in iter_stream(reader) {
        let item = match item {
            Ok(item) => item,
            Err(_e) => {
                // Stream parsing failed (e.g., incomplete data from interrupted run)
                // Return the partial results we've collected so far
                break;
            }
        };

        match item {
            ScannedItem::Unknown(_data, _err) => {
                // Incomplete or corrupted data at end of stream (e.g., from Ctrl+C)
                // Return the partial results we've collected so far
                break;
            }
            ScannedItem::Bytes(_) => {
                // Skip non-event data (this is normal in subunit streams)
                continue;
            }
            ScannedItem::Event(event) => {
                if let Some(ref test_id_str) = event.test_id {
                    // Track start events for duration calculation
                    if event.status == SubunitTestStatus::InProgress {
                        progress_callback(test_id_str, ProgressStatus::InProgress);
                        if let Some(timestamp) = event.timestamp {
                            let dt: chrono::DateTime<chrono::Utc> =
                                timestamp.try_into().map_err(|e| {
                                    Error::Subunit(format!(
                                        "Invalid timestamp in start event: {}",
                                        e
                                    ))
                                })?;
                            start_times.insert(test_id_str.clone(), dt);
                        }
                        continue;
                    }

                    // Convert subunit status to our TestStatus
                    let (status, progress_status) = match event.status {
                        SubunitTestStatus::Success => {
                            (TestStatus::Success, ProgressStatus::Success)
                        }
                        SubunitTestStatus::Failed => (TestStatus::Failure, ProgressStatus::Failed),
                        SubunitTestStatus::Skipped => (TestStatus::Skip, ProgressStatus::Skipped),
                        SubunitTestStatus::ExpectedFailure => {
                            (TestStatus::ExpectedFailure, ProgressStatus::ExpectedFailure)
                        }
                        SubunitTestStatus::UnexpectedSuccess => (
                            TestStatus::UnexpectedSuccess,
                            ProgressStatus::UnexpectedSuccess,
                        ),
                        SubunitTestStatus::Undefined
                        | SubunitTestStatus::Enumeration
                        | SubunitTestStatus::InProgress => {
                            continue;
                        }
                    };

                    progress_callback(test_id_str, progress_status);

                    let test_id = TestId::new(test_id_str.clone());

                    // Extract tags
                    let tags = event.tags.unwrap_or_default();

                    // Extract file content as message/details
                    let (message, details) = if let Some((_name, content)) = event.file.file {
                        let content_str = String::from_utf8_lossy(&content).to_string();
                        (Some(content_str.clone()), Some(content_str))
                    } else {
                        (None, None)
                    };

                    // Calculate duration from start/stop timestamps
                    let duration = if let (Some(start_time), Some(end_time)) =
                        (start_times.get(test_id_str), event.timestamp)
                    {
                        let end_time_chrono: chrono::DateTime<chrono::Utc> =
                            end_time.try_into().map_err(|e| {
                                Error::Subunit(format!("Invalid timestamp in end event: {}", e))
                            })?;
                        let duration_secs = (end_time_chrono - *start_time).num_milliseconds();
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
        }
    }
    Ok(test_run)
}

/// Parse a subunit stream into a TestRun
///
/// If the stream is incomplete or interrupted, returns partial results collected before the error.
/// Returns an error only for invalid timestamps in otherwise valid events.
pub fn parse_stream<R: Read>(reader: R, run_id: String) -> Result<TestRun> {
    use std::collections::HashMap;

    let mut test_run = TestRun::new(run_id);
    let mut start_times: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();

    // Iterate over the subunit stream
    for item in iter_stream(reader) {
        let item = match item {
            Ok(item) => item,
            Err(_e) => {
                // Stream parsing failed (e.g., incomplete data from interrupted run)
                // Return the partial results we've collected so far
                break;
            }
        };

        match item {
            ScannedItem::Unknown(_data, _err) => {
                // Incomplete or corrupted data at end of stream (e.g., from Ctrl+C)
                // Return the partial results we've collected so far
                break;
            }
            ScannedItem::Bytes(_) => {
                // Skip non-event data (this is normal in subunit streams)
                continue;
            }
            ScannedItem::Event(event) => {
                if let Some(ref test_id_str) = event.test_id {
                    // Track start events for duration calculation
                    if event.status == SubunitTestStatus::InProgress {
                        if let Some(timestamp) = event.timestamp {
                            let dt: chrono::DateTime<chrono::Utc> =
                                timestamp.try_into().map_err(|e| {
                                    Error::Subunit(format!(
                                        "Invalid timestamp in start event: {}",
                                        e
                                    ))
                                })?;
                            start_times.insert(test_id_str.clone(), dt);
                        }
                        continue; // Don't add inprogress events to results
                    }

                    // Convert subunit status to our TestStatus
                    let status = match event.status {
                        SubunitTestStatus::Success => TestStatus::Success,
                        SubunitTestStatus::Failed => TestStatus::Failure,
                        SubunitTestStatus::Skipped => TestStatus::Skip,
                        SubunitTestStatus::ExpectedFailure => TestStatus::ExpectedFailure,
                        SubunitTestStatus::UnexpectedSuccess => TestStatus::UnexpectedSuccess,
                        SubunitTestStatus::Undefined
                        | SubunitTestStatus::Enumeration
                        | SubunitTestStatus::InProgress => {
                            continue; // Skip events with these statuses
                        }
                    };

                    let test_id = TestId::new(test_id_str.clone());

                    // Extract tags
                    let tags = event.tags.unwrap_or_default();

                    // Extract file content as message/details
                    let (message, details) = if let Some((_name, content)) = event.file.file {
                        let content_str = String::from_utf8_lossy(&content).to_string();
                        (Some(content_str.clone()), Some(content_str))
                    } else {
                        (None, None)
                    };

                    // Calculate duration from start/stop timestamps
                    let duration = if let (Some(start_time), Some(end_time)) =
                        (start_times.get(test_id_str), event.timestamp)
                    {
                        let end_time_chrono: chrono::DateTime<chrono::Utc> =
                            end_time.try_into().map_err(|e| {
                                Error::Subunit(format!("Invalid timestamp in end event: {}", e))
                            })?;
                        let duration_secs = (end_time_chrono - *start_time).num_milliseconds();
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
        }
    }

    Ok(test_run)
}

/// Write a TestRun as a subunit stream
///
/// Returns an error if timestamp conversion fails or if the event is too large to serialize.
pub fn write_stream<W: Write>(test_run: &TestRun, mut writer: W) -> Result<()> {
    for result in test_run.results.values() {
        // If we have duration information, write an "inprogress" event first
        if let Some(duration) = result.duration {
            // Calculate start time by subtracting duration from run timestamp
            // Use seconds to avoid precision issues and chrono panics
            let duration_secs = duration.as_secs() as i64;
            let start_timestamp = test_run.timestamp - chrono::Duration::seconds(duration_secs);

            let mut start_event =
                Event::new(SubunitTestStatus::InProgress).test_id(result.test_id.as_str());

            start_event = start_event
                .datetime(start_timestamp)
                .map_err(|e| Error::Subunit(format!("Failed to set datetime: {}", e)))?;

            for tag in &result.tags {
                start_event = start_event.tag(tag);
            }

            start_event
                .build()
                .serialize(&mut writer)
                .map_err(|e| Error::Subunit(format!("Failed to write subunit event: {}", e)))?;
        }

        let status = match result.status {
            TestStatus::Success => SubunitTestStatus::Success,
            TestStatus::Failure => SubunitTestStatus::Failed,
            TestStatus::Error => SubunitTestStatus::Failed, // Subunit v2 doesn't have a separate 'error' status
            TestStatus::Skip => SubunitTestStatus::Skipped,
            TestStatus::ExpectedFailure => SubunitTestStatus::ExpectedFailure,
            TestStatus::UnexpectedSuccess => SubunitTestStatus::UnexpectedSuccess,
        };

        let mut event = Event::new(status).test_id(result.test_id.as_str());

        event = event
            .datetime(test_run.timestamp)
            .map_err(|e| Error::Subunit(format!("Failed to set datetime: {}", e)))?;

        for tag in &result.tags {
            event = event.tag(tag);
        }

        // Add details as file attachment if present
        if let Some(ref details) = result.details {
            event = event
                .mime_type("text/plain")
                .file_content("traceback", details.as_bytes());
        }

        // Write event - errors from subunit crate are properly handled
        event
            .build()
            .serialize(&mut writer)
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
        // Test that invalid UTF-8 or corrupted subunit data returns an error, not a panic
        // The new subunit-rust is more robust and treats plain text as valid (it's interleaved text),
        // so we need to use actually corrupted data
        let invalid_data: &[u8] = &[
            0xB2, // Start of subunit v2 signature
            0x9A, 0x00, // Incomplete/corrupted packet
            0xFF, 0xFF, 0xFF, // Invalid data
        ];
        let result = parse_stream(invalid_data, "0".to_string());

        // The key requirement is: no panic. Whether it returns an error or empty result
        // depends on how lenient the parser is. Both are acceptable.
        match result {
            Ok(run) => {
                // Parser was lenient and skipped the corrupted data - this is fine
                assert_eq!(run.total_tests(), 0);
            }
            Err(Error::Subunit(msg)) => {
                // Parser detected corruption - this is also fine
                assert!(
                    msg.contains("Invalid") || msg.contains("Failed to parse"),
                    "Error message: {}",
                    msg
                );
            }
            Err(e) => {
                panic!("Unexpected error type: {:?}", e);
            }
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
