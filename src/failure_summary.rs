//! One-line failure summaries for well-known test runner output
//!
//! Attachments are free-form, so the generic summary is just their last
//! non-empty line. For runners whose output format we know, this module picks
//! the line that actually describes the failure.

/// Summarise a failed test's attachments, given as `(name, content)` pairs in
/// arrival order. Returns `None` if no attachment is in a recognised format.
pub(crate) fn summarize(attachments: &[(String, String)]) -> Option<String> {
    attachments.iter().rev().find_map(|(name, content)| {
        python_traceback(name, content)
            .or_else(|| rust_panic(content))
            .or_else(|| go_test_output(content))
    })
}

/// testtools sends a Python traceback as a `traceback` attachment, or as
/// `traceback {description}` for each failed subtest. Its last line is the
/// exception.
fn python_traceback(name: &str, content: &str) -> Option<String> {
    if name != "traceback" && !name.starts_with("traceback ") {
        return None;
    }
    last_line(content)
}

/// libtest prints `thread 'NAME' (ID) panicked at FILE:LINE:COL:` followed by
/// the panic message, which may continue over several lines (e.g. the
/// `left`/`right` values of `assert_eq!`).
fn rust_panic(content: &str) -> Option<String> {
    let mut lines = content.lines();
    lines.find(|line| line.starts_with("thread '") && line.contains(" panicked at "))?;
    lines
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// `go test` reports a panic as `panic: MESSAGE`, and `t.Error`/`t.Fatal`
/// output as indented `FILE.go:LINE: MESSAGE` lines. `t.Log` output looks the
/// same, but the failing call is usually the last one.
fn go_test_output(content: &str) -> Option<String> {
    if let Some(panic) = content.lines().find(|line| line.starts_with("panic: ")) {
        return Some(panic.to_string());
    }
    content
        .lines()
        .rev()
        .filter(|line| line.starts_with(' ') || line.starts_with('\t'))
        .find_map(|line| {
            let (location, message) = line.trim_start().split_once(": ")?;
            let (file, line_no) = location.rsplit_once(':')?;
            if file.ends_with(".go") && line_no.parse::<u32>().is_ok() {
                Some(message.trim().to_string())
            } else {
                None
            }
        })
}

/// Last non-empty line of `text`, trimmed.
pub(crate) fn last_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachments(files: &[(&str, &str)]) -> Vec<(String, String)> {
        files
            .iter()
            .map(|(name, content)| (name.to_string(), content.to_string()))
            .collect()
    }

    #[test]
    fn python_takes_exception_line_of_last_traceback() {
        let files = attachments(&[
            (
                "traceback (i=2)",
                "Traceback (most recent call last):\n  File \"t.py\", line 6\nAssertionError: 2 != 1\n",
            ),
            (
                "traceback (i=3)",
                "Traceback (most recent call last):\n  File \"t.py\", line 6\nAssertionError: 3 != 1\n",
            ),
            ("stderr", "noise\n"),
        ]);
        assert_eq!(summarize(&files).as_deref(), Some("AssertionError: 3 != 1"));
    }

    #[test]
    fn rust_takes_panic_message() {
        let files = attachments(&[(
            "stdout",
            "\nthread 'eq_fails' (1815411) panicked at src/lib.rs:2:17:\n\
             assertion `left == right` failed: maths\n  left: 2\n right: 3\n\
             note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n",
        )]);
        assert_eq!(
            summarize(&files).as_deref(),
            Some("assertion `left == right` failed: maths")
        );
    }

    #[test]
    fn go_takes_last_error_message() {
        let files = attachments(&[(
            "go test output",
            "=== RUN   TestErrorf\n    x_test.go:6: some log line\n    \
             x_test.go:7: got 2, want 3\n--- FAIL: TestErrorf (0.00s)\n",
        )]);
        assert_eq!(summarize(&files).as_deref(), Some("got 2, want 3"));
    }

    #[test]
    fn go_takes_panic_line() {
        let files = attachments(&[(
            "go test output",
            "=== RUN   TestPanic\n--- FAIL: TestPanic (0.00s)\n\
             panic: assignment to entry in nil map [recovered, repanicked]\n\n\
             goroutine 10 [running]:\ntesting.tRunner.func1.2({0x6b9080, 0x6f10e0})\n\
             \t/usr/lib/go-1.27/src/testing/testing.go:2125 +0x232\n",
        )]);
        assert_eq!(
            summarize(&files).as_deref(),
            Some("panic: assignment to entry in nil map [recovered, repanicked]")
        );
    }

    #[test]
    fn unrecognised_output_has_no_summary() {
        let files = attachments(&[
            ("stdout", "something went wrong\n"),
            (
                "go test output",
                "=== RUN   TestSub\n--- FAIL: TestSub (0.00s)\n",
            ),
        ]);
        assert_eq!(summarize(&files), None);
    }

    #[test]
    fn last_line_skips_trailing_blank_lines() {
        assert_eq!(last_line("a\n  b  \n \n").as_deref(), Some("b"));
        assert_eq!(last_line("  \n \n"), None);
    }
}
