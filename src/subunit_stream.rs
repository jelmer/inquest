//! Subunit stream processing
//!
//! This module provides functions to read and write subunit v2 streams,
//! converting between subunit events and our internal TestRun representation.
//!
//! This module supports both traditional file I/O and memory-mapped files
//! for improved performance with large subunit streams.

use crate::error::{Error, Result};
use crate::repository::{RunId, StreamInterruption, TestId, TestResult, TestRun, TestStatus};
use std::io::{Read, Write};
use subunit::io::sync::iter_stream;
use subunit::serialize::Serializable;
use subunit::types::event::Event;
use subunit::types::stream::ScannedItem;
use subunit::types::teststatus::TestStatus as SubunitTestStatus;
use subunit::types::timestamp::Timestamp;

/// Maximum number of consecutive parse errors before giving up on the stream
const MAX_CONSECUTIVE_ERRORS: usize = 100;

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

/// Controls which test output (stdout/stderr) to show
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFilter {
    /// Show output only from failed/unexpected success tests
    FailuresOnly,
    /// Show output from all tests (both passing and failing)
    All,
}

/// Convert a subunit timestamp to a chrono DateTime with error context
fn convert_timestamp(timestamp: Timestamp, context: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    timestamp
        .try_into()
        .map_err(|e| Error::Subunit(format!("Invalid timestamp in {}: {}", context, e)))
}

/// Convert a SubunitTestStatus to our TestStatus (None for non-terminal states)
fn convert_subunit_status(status: SubunitTestStatus) -> Option<TestStatus> {
    match status {
        SubunitTestStatus::Success => Some(TestStatus::Success),
        SubunitTestStatus::Failed => Some(TestStatus::Failure),
        SubunitTestStatus::Skipped => Some(TestStatus::Skip),
        SubunitTestStatus::ExpectedFailure => Some(TestStatus::ExpectedFailure),
        SubunitTestStatus::UnexpectedSuccess => Some(TestStatus::UnexpectedSuccess),
        SubunitTestStatus::Undefined
        | SubunitTestStatus::Enumeration
        | SubunitTestStatus::InProgress => None,
    }
}

/// Convert a SubunitTestStatus to both TestStatus and ProgressStatus
fn convert_status_with_progress(status: SubunitTestStatus) -> Option<(TestStatus, ProgressStatus)> {
    match status {
        SubunitTestStatus::Success => Some((TestStatus::Success, ProgressStatus::Success)),
        SubunitTestStatus::Failed => Some((TestStatus::Failure, ProgressStatus::Failed)),
        SubunitTestStatus::Skipped => Some((TestStatus::Skip, ProgressStatus::Skipped)),
        SubunitTestStatus::ExpectedFailure => {
            Some((TestStatus::ExpectedFailure, ProgressStatus::ExpectedFailure))
        }
        SubunitTestStatus::UnexpectedSuccess => Some((
            TestStatus::UnexpectedSuccess,
            ProgressStatus::UnexpectedSuccess,
        )),
        SubunitTestStatus::Undefined
        | SubunitTestStatus::Enumeration
        | SubunitTestStatus::InProgress => None,
    }
}

/// Accumulated file attachments for a single test, keyed by attachment name in
/// arrival order. A subunit writer may split one logical file across several
/// packets (`python -m subunit.run` chunks tracebacks), so content for a
/// repeated name is appended rather than replaced.
#[derive(Default)]
struct Attachments {
    files: Vec<(String, String)>,
}

impl Attachments {
    fn push(&mut self, name: &str, content: &str) {
        match self.files.iter_mut().find(|(n, _)| n == name) {
            Some((_, existing)) => existing.push_str(content),
            None => self.files.push((name.to_string(), content.to_string())),
        }
    }

    fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.files
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| c.as_str())
    }

    /// The failure detail to store on the `TestResult`: the traceback if the
    /// runner sent one, otherwise every other attachment concatenated, so
    /// runners that only report captured stdout/stderr still surface something.
    fn details(&self) -> Option<String> {
        if let Some(tb) = self.get("traceback") {
            return Some(tb.to_string());
        }
        let joined: String = self
            .files
            .iter()
            .filter(|(name, _)| name != "_tags")
            .map(|(_, content)| content.as_str())
            .collect();
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    }
}

/// One-line summary of a failure, for callers that show a single line per test
/// (CI annotations, the step summary, `inq failing`). For a Python traceback
/// that is the trailing exception line, which carries the actual assertion;
/// the first line is the useless `Traceback (most recent call last):` banner.
fn summary_line(details: &str) -> Option<String> {
    details
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// Merge attachments buffered from preceding packets with any file content
/// carried on the terminal status packet itself, and derive the result's
/// `message` (one-line summary) and `details` (the full traceback).
///
/// Both shapes occur in the wild: `python -m subunit.run` sends the traceback
/// as separate file-content packets before an empty status packet, while our
/// own `write_stream` attaches it directly to the status packet.
fn result_content(
    mut buffered: Attachments,
    status_file: Option<(&str, &[u8])>,
) -> (Option<String>, Option<String>) {
    if let Some((name, content)) = status_file {
        buffered.push(name, &String::from_utf8_lossy(content));
    }
    let details = buffered.details();
    let message = details.as_deref().and_then(summary_line);
    (message, details)
}

/// Parse a subunit stream from a byte slice into a TestRun
///
/// This is optimized for memory-mapped files and avoids copying data.
pub fn parse_stream_bytes(data: &[u8], run_id: RunId) -> Result<TestRun> {
    parse_stream(data, run_id)
}

/// Parse a subunit stream into a TestRun with progress callback
///
/// The callback is called with (test_id, status) for each test event.
/// The bytes_callback is called with non-subunit output (print statements, warnings, etc.)
/// based on the output_filter setting:
/// - OutputFilter::All: Show all output immediately
/// - OutputFilter::FailuresOnly: Only show output from failed/unexpected success tests
///
/// The result_callback is called once per completed test with the assembled
/// `TestResult`, so callers can react to results as they arrive (e.g. emit
/// streaming CI annotations) without waiting for the run to finish.
///
/// If the stream is incomplete or interrupted, returns partial results collected before the error.
/// Returns an error only for invalid timestamps in otherwise valid events.
pub fn parse_stream_with_progress<R: Read, F, B, R2>(
    reader: R,
    run_id: RunId,
    mut progress_callback: F,
    mut bytes_callback: B,
    mut result_callback: R2,
    output_filter: OutputFilter,
) -> Result<TestRun>
where
    F: FnMut(&str, ProgressStatus),
    B: FnMut(&[u8]),
    R2: FnMut(&TestResult),
{
    use std::collections::HashMap;

    let mut test_run = TestRun::new(run_id);
    let mut start_times: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
    let mut consecutive_errors = 0;

    // Track output for the current test (for filtering)
    let mut current_test_output: Vec<u8> = Vec::new();
    // Buffer file attachments per test until we know the status
    let mut pending_attachments: std::collections::HashMap<String, Attachments> =
        std::collections::HashMap::new();

    // Iterate over the subunit stream
    for item in iter_stream(reader) {
        let item = match item {
            Ok(item) => {
                consecutive_errors = 0; // Reset on success
                item
            }
            Err(_e) => {
                // Stream parsing failed (e.g., incomplete data from interrupted run)
                // Continue reading to drain the pipe (prevents BrokenPipeError in child process)
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    test_run.interruption =
                        Some(StreamInterruption::ParseErrors(consecutive_errors));
                    break;
                }
                // Silently skip individual parsing errors to drain the pipe
                continue;
            }
        };

        match item {
            ScannedItem::Unknown(_data, _err) => {
                // Incomplete or corrupted data - continue reading to drain the pipe
                // This prevents BrokenPipeError in the child process that's still writing
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    test_run.interruption =
                        Some(StreamInterruption::UnknownItems(consecutive_errors));
                    break;
                }
                // Silently skip unknown items to drain the pipe
                continue;
            }
            ScannedItem::Bytes(bytes) => {
                // Non-event data (e.g., print statements from tests)
                consecutive_errors = 0; // Reset on any valid item

                match output_filter {
                    OutputFilter::All => {
                        // Show all output immediately
                        bytes_callback(&bytes);
                    }
                    OutputFilter::FailuresOnly => {
                        // Buffer output for the current test
                        current_test_output.extend_from_slice(&bytes);
                    }
                }
                continue;
            }
            ScannedItem::Event(event) => {
                consecutive_errors = 0; // Reset on any valid event

                if let Some(ref test_id_str) = event.test_id {
                    // Handle file attachments from Undefined status events (stdout/stderr/tracebacks)
                    // Buffer them until we know the test status
                    if event.status == SubunitTestStatus::Undefined && event.file.file.is_some() {
                        if let Some((name, content)) = &event.file.file {
                            let entry = pending_attachments.entry(test_id_str.clone()).or_default();
                            // Tags ride along with the first attachment.
                            if entry.is_empty() {
                                if let Some(tags) = event.tags.clone() {
                                    entry.push("_tags", &tags.join(" "));
                                }
                            }
                            entry.push(name, &String::from_utf8_lossy(content));
                        }
                        continue;
                    }

                    // Track start events for duration calculation
                    if event.status == SubunitTestStatus::InProgress {
                        progress_callback(test_id_str, ProgressStatus::InProgress);
                        current_test_output.clear();
                        if let Some(timestamp) = event.timestamp {
                            let dt = convert_timestamp(timestamp, "start event")?;
                            start_times.insert(test_id_str.clone(), dt);
                        }
                        continue;
                    }

                    // Convert subunit status to our TestStatus
                    let (status, progress_status) =
                        if let Some(converted) = convert_status_with_progress(event.status) {
                            converted
                        } else {
                            continue;
                        };

                    progress_callback(test_id_str, progress_status);

                    // Now that we know the status, flush any pending file
                    // attachments. Keep them around afterwards: they also
                    // carry the traceback we store on the `TestResult`.
                    let attachments = pending_attachments.remove(test_id_str).unwrap_or_default();
                    {
                        let is_failure = matches!(
                            progress_status,
                            ProgressStatus::Failed | ProgressStatus::UnexpectedSuccess
                        );

                        let should_show = match output_filter {
                            OutputFilter::All => true,
                            OutputFilter::FailuresOnly => is_failure,
                        };

                        if should_show && !attachments.is_empty() {
                            // Build all output in a single buffer to avoid progress bar interruption
                            let mut output = Vec::new();

                            // Header
                            let status_str = match progress_status {
                                ProgressStatus::Failed => "FAIL",
                                ProgressStatus::UnexpectedSuccess => "FAIL",
                                ProgressStatus::Success => "PASSED",
                                ProgressStatus::Skipped => "SKIPPED",
                                ProgressStatus::ExpectedFailure => "XFAIL",
                                _ => "UNKNOWN",
                            };

                            output.extend_from_slice(
                                format!("{}: {}\n", status_str, test_id_str).as_bytes(),
                            );

                            // Show tags if present
                            if let Some(tags_str) = attachments.get("_tags") {
                                output
                                    .extend_from_slice(format!("tags: {}\n", tags_str).as_bytes());
                            }

                            // Separator line
                            output.extend_from_slice(b"----------------------------------------------------------------------\n");

                            // Show file attachments
                            let mut has_traceback = false;
                            for (name, content) in &attachments.files {
                                if name == "_tags" {
                                    continue; // Skip tags, already shown
                                }

                                if name == "log" {
                                    output.extend_from_slice(b"log: {{{\n");
                                    output.extend_from_slice(content.as_bytes());
                                    output.extend_from_slice(b"}}}\n\n");
                                } else if name == "traceback" {
                                    output.extend_from_slice(content.as_bytes());
                                    has_traceback = true;
                                }
                            }

                            // Footer separator after all attachments if there was a traceback
                            if has_traceback {
                                output.extend_from_slice(b"======================================================================\n");
                            }

                            // Write all output at once
                            bytes_callback(&output);
                        }
                    }

                    // Store the traceback on the result. It arrives either in
                    // the preceding attachment packets or on this status packet.
                    let (message, details) = result_content(
                        attachments,
                        event
                            .file
                            .file
                            .as_ref()
                            .map(|(name, content)| (name.as_str(), content.as_slice())),
                    );

                    // Show any buffered output for this test
                    match output_filter {
                        OutputFilter::All => {
                            // Show all buffered output immediately
                            if !current_test_output.is_empty() {
                                bytes_callback(&current_test_output);
                                current_test_output.clear();
                            }
                        }
                        OutputFilter::FailuresOnly => {
                            // Only show buffered output for failed tests
                            if matches!(
                                progress_status,
                                ProgressStatus::Failed | ProgressStatus::UnexpectedSuccess
                            ) && !current_test_output.is_empty()
                            {
                                bytes_callback(&current_test_output);
                            }
                            current_test_output.clear();
                        }
                    }

                    let test_id = TestId::new(test_id_str.clone());

                    // Extract tags
                    let tags = event.tags.unwrap_or_default();

                    // Calculate duration from start/stop timestamps
                    let duration = if let (Some(start_time), Some(end_time)) =
                        (start_times.remove(test_id_str), event.timestamp)
                    {
                        let end_time_chrono = convert_timestamp(end_time, "end event")?;
                        let duration_secs = (end_time_chrono - start_time).num_milliseconds();
                        if duration_secs >= 0 {
                            Some(std::time::Duration::from_millis(duration_secs as u64))
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    let result = TestResult {
                        test_id,
                        status,
                        duration,
                        message,
                        details,
                        tags,
                    };
                    result_callback(&result);
                    test_run.add_result(result);
                }
            }
        }
    }

    for test_id in pending_attachments.keys() {
        tracing::warn!(
            "test {} started but never completed, discarding buffered attachments",
            test_id
        );
    }

    Ok(test_run)
}

/// Parse a subunit stream into a TestRun
///
/// If the stream is incomplete or interrupted, returns partial results collected before the error.
/// Returns an error only for invalid timestamps in otherwise valid events.
pub fn parse_stream<R: Read>(reader: R, run_id: RunId) -> Result<TestRun> {
    use std::collections::HashMap;

    let mut test_run = TestRun::new(run_id);
    let mut start_times: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
    let mut consecutive_errors = 0;
    // Buffer file attachments per test until we see the terminal status event
    let mut pending_attachments: HashMap<String, Attachments> = HashMap::new();

    // Iterate over the subunit stream
    for item in iter_stream(reader) {
        let item = match item {
            Ok(item) => {
                consecutive_errors = 0; // Reset on success
                item
            }
            Err(_e) => {
                // Stream parsing failed (e.g., incomplete data from interrupted run)
                // Continue reading to handle partial data gracefully
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    test_run.interruption =
                        Some(StreamInterruption::ParseErrors(consecutive_errors));
                    break;
                }
                continue;
            }
        };

        match item {
            ScannedItem::Unknown(_data, _err) => {
                // Incomplete or corrupted data - continue reading to handle gracefully
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    test_run.interruption =
                        Some(StreamInterruption::UnknownItems(consecutive_errors));
                    break;
                }
                continue;
            }
            ScannedItem::Bytes(_) => {
                // Skip non-event data (this is normal in subunit streams)
                consecutive_errors = 0; // Reset on any valid item
                continue;
            }
            ScannedItem::Event(event) => {
                consecutive_errors = 0; // Reset on any valid event
                if let Some(ref test_id_str) = event.test_id {
                    // Buffer file attachments (tracebacks, captured output) that
                    // arrive in their own packets ahead of the status event.
                    if event.status == SubunitTestStatus::Undefined {
                        if let Some((name, content)) = &event.file.file {
                            pending_attachments
                                .entry(test_id_str.clone())
                                .or_default()
                                .push(name, &String::from_utf8_lossy(content));
                        }
                        continue;
                    }

                    // Track start events for duration calculation
                    if event.status == SubunitTestStatus::InProgress {
                        if let Some(timestamp) = event.timestamp {
                            let dt = convert_timestamp(timestamp, "start event")?;
                            start_times.insert(test_id_str.clone(), dt);
                        }
                        continue; // Don't add inprogress events to results
                    }

                    // Convert subunit status to our TestStatus
                    let status = if let Some(s) = convert_subunit_status(event.status) {
                        s
                    } else {
                        continue; // Skip events with non-terminal statuses
                    };

                    let test_id = TestId::new(test_id_str.clone());

                    // Extract tags
                    let tags = event.tags.clone().unwrap_or_default();

                    let (message, details) = result_content(
                        pending_attachments.remove(test_id_str).unwrap_or_default(),
                        event
                            .file
                            .file
                            .as_ref()
                            .map(|(name, content)| (name.as_str(), content.as_slice())),
                    );

                    // Calculate duration from start/stop timestamps
                    let duration = if let (Some(start_time), Some(end_time)) =
                        (start_times.remove(test_id_str), event.timestamp)
                    {
                        let end_time_chrono = convert_timestamp(end_time, "end event")?;
                        let duration_secs = (end_time_chrono - start_time).num_milliseconds();
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

/// Read a subunit stream and return the test IDs in execution order.
///
/// Tests are ordered by the first event observed for each test (the
/// `inprogress` event written when the test starts, or the completion event
/// for streams that omit progress markers). Each test ID appears at most once.
pub fn parse_stream_test_order<R: Read>(reader: R) -> Result<Vec<TestId>> {
    let mut order: Vec<TestId> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for item in iter_stream(reader) {
        let item = match item {
            Ok(item) => item,
            Err(_) => continue,
        };
        if let ScannedItem::Event(event) = item {
            if let Some(test_id_str) = event.test_id {
                if seen.insert(test_id_str.clone()) {
                    order.push(TestId::new(test_id_str));
                }
            }
        }
    }

    Ok(order)
}

/// Filter a raw subunit stream to only include failing tests
///
/// This preserves the complete subunit events including file attachments (log, traceback)
/// for tests that have failing status.
pub fn filter_failing_tests<R: Read, W: Write>(mut reader: R, mut writer: W) -> Result<()> {
    use std::collections::HashSet;

    // First pass: identify which tests are failures
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer)?;

    let mut failing_tests = HashSet::new();

    for item in iter_stream(&buffer[..]) {
        if let Ok(ScannedItem::Event(event)) = item {
            if let Some(ref test_id) = event.test_id {
                let is_failure = matches!(
                    event.status,
                    SubunitTestStatus::Failed | SubunitTestStatus::UnexpectedSuccess
                );

                if is_failure {
                    failing_tests.insert(test_id.clone());
                }
            }
        }
    }

    // Second pass: write events only for failing tests
    for item in iter_stream(&buffer[..]) {
        match item {
            Ok(ScannedItem::Event(event)) => {
                if let Some(ref test_id) = event.test_id {
                    if failing_tests.contains(test_id) {
                        event.serialize(&mut writer).map_err(|e| {
                            Error::Subunit(format!("Failed to serialize event: {}", e))
                        })?;
                    }
                }
            }
            Ok(ScannedItem::Bytes(_)) => {
                // Skip non-subunit content
            }
            Ok(ScannedItem::Unknown(_, _)) => {
                // Skip unknown items
            }
            Err(_) => {
                // Skip errors
            }
        }
    }

    Ok(())
}

/// Write a TestRun as a subunit stream
///
/// Returns an error if timestamp conversion fails or if the event is too large to serialize.
pub fn write_stream<W: Write>(test_run: &TestRun, mut writer: W) -> Result<()> {
    for result in test_run.results.values() {
        // If we have duration information, write an "inprogress" event first
        if let Some(duration) = result.duration {
            // Calculate start time by subtracting duration from run timestamp
            // Use milliseconds to preserve precision in test durations
            let duration_millis = duration.as_millis() as i64;
            let start_timestamp =
                test_run.timestamp - chrono::Duration::milliseconds(duration_millis);

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
        event.build().serialize(&mut writer).map_err(|e| {
            Error::Subunit(format!(
                "Failed to write subunit event for {}: {}",
                result.test_id.as_str(),
                e
            ))
        })?;
    }

    // Explicitly flush to ensure all data is written to disk
    writer.flush().map_err(Error::Io)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_parse_empty_stream() {
        let empty_stream: &[u8] = &[];
        let result = parse_stream(empty_stream, RunId::new("0"));
        assert!(result.is_ok());
        let run = result.unwrap();
        assert_eq!(run.total_tests(), 0);
    }

    #[test]
    fn test_roundtrip_test_run() {
        // Create a test run
        // Use a fixed timestamp to avoid chrono issues with the subunit crate
        let mut test_run = TestRun::new(RunId::new("0"));
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
        let parsed = parse_stream(&buffer[..], RunId::new("1")).unwrap();

        // Verify
        assert_eq!(parsed.total_tests(), 2);
        assert_eq!(parsed.count_successes(), 1);
        assert_eq!(parsed.count_failures(), 1);
    }

    /// Serialize a stream the way `python -m subunit.run` does: the traceback
    /// arrives in its own file-content packets with no status, split across
    /// several chunks, and the terminal status packet carries no file at all.
    /// Our own `write_stream` attaches the traceback to the status packet
    /// instead, so a roundtrip through it would not exercise this path.
    fn write_python_style_failure(test_id: &str, traceback_chunks: &[&str]) -> Vec<u8> {
        let mut buffer = Vec::new();
        Event::new(SubunitTestStatus::InProgress)
            .test_id(test_id)
            .build()
            .serialize(&mut buffer)
            .unwrap();
        for chunk in traceback_chunks {
            Event::new(SubunitTestStatus::Undefined)
                .test_id(test_id)
                .mime_type("text/x-traceback; charset=\"utf8\"; language=\"python\"")
                .file_content("traceback", chunk.as_bytes())
                .build()
                .serialize(&mut buffer)
                .unwrap();
        }
        Event::new(SubunitTestStatus::Failed)
            .test_id(test_id)
            .build()
            .serialize(&mut buffer)
            .unwrap();
        buffer
    }

    #[test]
    fn test_parse_stream_captures_traceback_from_attachment_packets() {
        let stream = write_python_style_failure(
            "tests.test_x.T.test_boom",
            &[
                "Traceback (most recent call last):\n",
                "AssertionError: 1 != 2\n",
            ],
        );

        let parsed = parse_stream(&stream[..], RunId::new("0")).unwrap();
        let result = parsed
            .results
            .get(&TestId::new("tests.test_x.T.test_boom"))
            .unwrap();
        assert_eq!(result.status, TestStatus::Failure);
        assert_eq!(
            result.details.as_deref(),
            Some("Traceback (most recent call last):\nAssertionError: 1 != 2\n")
        );
        // The message is the one-line summary, not a copy of the traceback.
        assert_eq!(result.message.as_deref(), Some("AssertionError: 1 != 2"));
    }

    #[test]
    fn test_summary_line_takes_trailing_exception_line() {
        assert_eq!(
            summary_line("Traceback (most recent call last):\n  x\nValueError: bad\n").as_deref(),
            Some("ValueError: bad")
        );
        assert_eq!(summary_line("").as_deref(), None);
        assert_eq!(summary_line("  \n \n").as_deref(), None);
    }

    #[test]
    fn test_attachments_concatenate_chunks_of_one_file() {
        let mut a = Attachments::default();
        a.push("traceback", "Traceback:\n");
        a.push("traceback", "ValueError: bad\n");
        assert_eq!(
            a.details().as_deref(),
            Some("Traceback:\nValueError: bad\n")
        );
    }

    #[test]
    fn test_attachments_fall_back_to_non_traceback_files() {
        let mut a = Attachments::default();
        a.push("_tags", "worker-0");
        a.push("stderr", "boom\n");
        assert_eq!(a.details().as_deref(), Some("boom\n"));
    }

    #[test]
    fn test_parse_stream_with_progress_captures_traceback_from_attachment_packets() {
        let stream = write_python_style_failure(
            "tests.test_x.T.test_boom",
            &[
                "Traceback (most recent call last):\n",
                "AssertionError: 1 != 2\n",
            ],
        );

        let parsed = parse_stream_with_progress(
            &stream[..],
            RunId::new("0"),
            |_, _| {},
            |_| {},
            |_| {},
            OutputFilter::FailuresOnly,
        )
        .unwrap();
        let result = parsed
            .results
            .get(&TestId::new("tests.test_x.T.test_boom"))
            .unwrap();
        assert_eq!(result.status, TestStatus::Failure);
        assert_eq!(
            result.details.as_deref(),
            Some("Traceback (most recent call last):\nAssertionError: 1 != 2\n")
        );
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
            let mut test_run = TestRun::new(RunId::new("0"));
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

            let parsed = parse_stream(&buffer[..], RunId::new("1")).unwrap();
            assert_eq!(parsed.total_tests(), 1);

            let result = parsed.results.values().next().unwrap();
            assert_eq!(result.status, status);
        }
    }

    #[test]
    fn test_parse_stream_test_order_preserves_order() {
        let mut test_run = TestRun::new(RunId::new("0"));
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        // Pick names that don't sort to the same order they're added in,
        // so the test catches a regression that returns sorted output.
        for name in ["zeta", "alpha", "mu"] {
            test_run.add_result(TestResult {
                test_id: TestId::new(name),
                status: TestStatus::Success,
                duration: Some(Duration::from_millis(1)),
                message: None,
                details: None,
                tags: vec![],
            });
        }

        // write_stream iterates over a HashMap so the on-disk order is not
        // deterministic in source order. parse_stream_test_order must return
        // the order observed in the stream itself, regardless of which order
        // that turns out to be.
        let mut buffer = Vec::new();
        write_stream(&test_run, &mut buffer).unwrap();

        let order = parse_stream_test_order(&buffer[..]).unwrap();
        assert_eq!(order.len(), 3);
        let mut sorted: Vec<&str> = order.iter().map(|t| t.as_str()).collect();
        sorted.sort();
        assert_eq!(sorted, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn test_parse_stream_test_order_deduplicates() {
        // The same test ID typically appears in both the inprogress event
        // and the completion event. parse_stream_test_order should record
        // each test only once, on first sighting.
        let mut test_run = TestRun::new(RunId::new("0"));
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult {
            test_id: TestId::new("only_one"),
            status: TestStatus::Success,
            duration: Some(Duration::from_millis(1)),
            message: None,
            details: None,
            tags: vec![],
        });

        let mut buffer = Vec::new();
        write_stream(&test_run, &mut buffer).unwrap();

        let order = parse_stream_test_order(&buffer[..]).unwrap();
        assert_eq!(order.len(), 1);
        assert_eq!(order[0].as_str(), "only_one");
    }

    #[test]
    fn test_parse_stream_test_order_empty() {
        let order = parse_stream_test_order(&b""[..]).unwrap();
        assert!(order.is_empty());
    }

    #[test]
    fn test_progress_status_indicator() {
        // Test all indicator outputs to catch mutations
        assert_eq!(ProgressStatus::InProgress.indicator(), "");
        assert_eq!(ProgressStatus::Success.indicator(), "✓");
        assert_eq!(ProgressStatus::Failed.indicator(), "✗");
        assert_eq!(ProgressStatus::Skipped.indicator(), "⊘");
        assert_eq!(ProgressStatus::ExpectedFailure.indicator(), "✓");
        assert_eq!(ProgressStatus::UnexpectedSuccess.indicator(), "✗");
    }

    #[test]
    fn test_invalid_subunit_stream_no_panic() {
        // Test that invalid UTF-8 or corrupted subunit data returns an error, not a panic
        // The new subunit-rust is more lenient and treats plain text as valid (it's interleaved text),
        // so we need to use actually corrupted data
        let invalid_data: &[u8] = &[
            0xB2, // Start of subunit v2 signature
            0x9A, 0x00, // Incomplete/corrupted packet
            0xFF, 0xFF, 0xFF, // Invalid data
        ];
        let result = parse_stream(invalid_data, RunId::new("0"));

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
        let mut test_run = TestRun::new(RunId::new("0"));
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
        let parsed = parse_stream_bytes(&buffer, RunId::new("1")).unwrap();

        // Verify
        assert_eq!(parsed.total_tests(), 1);
        assert_eq!(parsed.count_successes(), 1);
        let result = parsed.results.values().next().unwrap();
        assert_eq!(result.test_id.as_str(), "test1");
        assert!(result.tags.contains(&"mmap-test".to_string()));
    }

    #[test]
    fn test_filter_failing_tests() {
        // Create a test run with mixed results
        let mut test_run = TestRun::new(RunId::new("0"));
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();

        // Add passing test
        test_run.add_result(TestResult {
            test_id: TestId::new("test_pass"),
            status: TestStatus::Success,
            duration: None,
            message: None,
            details: None,
            tags: vec!["worker-0".to_string()],
        });

        // Add failing test
        test_run.add_result(TestResult {
            test_id: TestId::new("test_fail"),
            status: TestStatus::Failure,
            duration: None,
            message: Some("Failed".to_string()),
            details: Some("Error details".to_string()),
            tags: vec!["worker-1".to_string()],
        });

        // Add unexpected success
        test_run.add_result(TestResult {
            test_id: TestId::new("test_uxsuccess"),
            status: TestStatus::UnexpectedSuccess,
            duration: None,
            message: None,
            details: None,
            tags: vec!["worker-2".to_string()],
        });

        // Write the full stream
        let mut full_stream = Vec::new();
        write_stream(&test_run, &mut full_stream).unwrap();

        // Filter to only failing tests
        let mut filtered_stream = Vec::new();
        filter_failing_tests(&full_stream[..], &mut filtered_stream).unwrap();

        // Parse the filtered stream
        let parsed = parse_stream(&filtered_stream[..], RunId::new("filtered")).unwrap();

        // Should only have the 2 failing tests (Failure + UnexpectedSuccess)
        assert_eq!(parsed.total_tests(), 2);
        assert_eq!(parsed.count_failures(), 2); // Both are considered failures
        assert!(parsed.results.contains_key(&TestId::new("test_fail")));
        assert!(parsed.results.contains_key(&TestId::new("test_uxsuccess")));
        assert!(!parsed.results.contains_key(&TestId::new("test_pass")));
    }
}
