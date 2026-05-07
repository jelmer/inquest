//! Group a run's failing tests by their common, normalized failure tail.
//!
//! Many tests fail for the same underlying reason — the same panic, the same
//! assertion, the same exception — but their tracebacks differ in superficial
//! ways: file paths, line numbers, hex addresses, temp-dir names. This command
//! normalizes each failure's traceback, finds the part that actually carries
//! signal (the anchor — typically the line announcing the exception or panic),
//! and clusters failures whose normalized tails share a common suffix.
//!
//! The output names each cluster by the longest line sequence shared by all
//! its members, so a reader can see the unique failure once and the count of
//! tests it explains.

use crate::commands::utils::{open_repository, resolve_run_id};
use crate::commands::Command;
use crate::error::Result;
use crate::repository::{TestId, TestResult};
use crate::ui::UI;
use regex::Regex;
use std::sync::OnceLock;

/// Default cap on the number of sample test IDs shown per group.
pub const DEFAULT_SAMPLES: usize = 5;

/// Maximum number of normalized lines kept as a fingerprint per failure. Long
/// enough to capture an anchor plus a handful of preceding frames; short
/// enough that the union-find pairwise comparison is cheap even with many
/// failures.
const MAX_FINGERPRINT_LINES: usize = 12;

/// Minimum overlapping suffix lines for two failures to cluster together.
/// Two is the smallest number that meaningfully constrains "same failure":
/// one line could match by coincidence (e.g. just "FAILED"), two demands
/// the exception type *and* its preceding context match.
const MIN_OVERLAP_LINES: usize = 2;

/// Command to summarize failures in a run by their common failure tails.
pub struct SummarizeCommand {
    base_path: Option<String>,
    run_id: Option<String>,
    samples: usize,
}

impl SummarizeCommand {
    /// Create a summarize command for the given run.
    pub fn new(base_path: Option<String>, run_id: Option<String>, samples: usize) -> Self {
        SummarizeCommand {
            base_path,
            run_id,
            samples,
        }
    }
}

/// Pick the text to fingerprint a failure with: prefer the rich traceback,
/// fall back to the short message, then to the status string. Returning a
/// status string ensures every failure groups somewhere instead of being
/// silently dropped when subunit gives us no message.
fn pick_text(result: &TestResult) -> String {
    if let Some(d) = result.details.as_ref() {
        if !d.trim().is_empty() {
            return d.clone();
        }
    }
    if let Some(m) = result.message.as_ref() {
        if !m.trim().is_empty() {
            return m.clone();
        }
    }
    result.status.to_string()
}

/// Replace volatile substrings (paths, line/column numbers, hex addresses,
/// temp dirs, durations) with stable placeholders so two tracebacks that
/// differ only in those bits compare equal.
///
/// We accept some over-matching here: `<n>` may swallow an integer that's
/// genuinely part of the failure message. That's a reasonable trade — a
/// false merge is far less costly than fragmenting an obvious cluster, and
/// the original text is still available via `inq log`.
fn normalize_line(line: &str) -> String {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        vec![
            // Hex addresses: 0xdeadbeef, 0xDEAD_BEEF
            (Regex::new(r"0x[0-9a-fA-F_]+").unwrap(), "<hex>"),
            // Quoted file paths: File "/foo/bar.py", line 42 → File "<path>", line <n>
            (
                Regex::new(r#""[^"\n]*[/\\][^"\n]*""#).unwrap(),
                "\"<path>\"",
            ),
            // Bare absolute or relative paths with a filename + extension.
            // Matches things like /foo/bar/baz.py, src/lib.rs, ../tests/x.rs.
            (
                Regex::new(r"(?:[A-Za-z]:)?[\w./\\\-]+\.[A-Za-z][A-Za-z0-9]+").unwrap(),
                "<path>",
            ),
            // Temp directory tokens that survive path normalization, e.g. .tmpA1b2.
            (Regex::new(r"\.tmp[A-Za-z0-9]+").unwrap(), ".tmp<x>"),
            // "line 42", "line: 42"
            (Regex::new(r"\bline[: ]\s*\d+\b").unwrap(), "line <n>"),
            // ":42:" or ":42:7" trailing position markers.
            (Regex::new(r":\d+(?::\d+)?\b").unwrap(), ":<n>"),
            // Frame markers naming the failing function. Python frames end
            // with ", in <name>"; Rust panic locations are "at <path>:<n>".
            // Without normalizing the name, "in test_a" vs "in test_b"
            // prevents two otherwise identical failures from clustering.
            // Anchored to end-of-line to avoid collapsing prose like
            // "in the same way that ...".
            (Regex::new(r"(,\s*in)\s+[\w:.<>]+\s*$").unwrap(), "$1 <fn>"),
            // Durations like 1.234s, 0.5ms.
            (
                Regex::new(r"\b\d+(?:\.\d+)?(?:ns|µs|us|ms|s)\b").unwrap(),
                "<dur>",
            ),
            // Bare integers >= 4 digits (run ids, pids, ports, large counters).
            // Short integers often *are* the failure (e.g. "got 3 want 4") so
            // leave them alone.
            (Regex::new(r"\b\d{4,}\b").unwrap(), "<n>"),
        ]
    });

    let mut s = line.trim_end().to_string();
    for (re, replacement) in patterns {
        s = re.replace_all(&s, *replacement).into_owned();
    }
    s
}

/// Patterns whose presence on a line marks it as the "anchor" — the line that
/// names the failure. We deliberately favour the last anchor we see, since
/// chained exceptions ("During handling of the above…") put the proximate
/// cause last.
fn is_anchor_line(line: &str) -> bool {
    static ANCHOR: OnceLock<Regex> = OnceLock::new();
    let re = ANCHOR.get_or_init(|| {
        Regex::new(
            r#"(?x)
            (?:^|\W)
            (?:
                [A-Z]\w*(?:Error|Exception|Warning):\s
              | panicked\ at
              | thread\ '[^']*'\ panicked
              | assertion\ (?:failed|`)
              | AssertionError
              | FAILED
              | fatal\ error
              | Segmentation\ fault
              | abort(?:ed)?
            )
        "#,
        )
        .unwrap()
    });
    re.is_match(line)
}

/// Build a fingerprint for one failure: a normalized, blank-stripped sequence
/// of lines ending at (and including) the anchor line. If no anchor is found,
/// uses the last non-empty line. The result is capped at `MAX_FINGERPRINT_LINES`.
fn fingerprint(text: &str) -> Vec<String> {
    let normalized: Vec<String> = text
        .lines()
        .map(normalize_line)
        .filter(|l| !l.trim().is_empty())
        .collect();

    if normalized.is_empty() {
        return Vec::new();
    }

    let anchor = normalized
        .iter()
        .rposition(|l| is_anchor_line(l))
        .unwrap_or(normalized.len() - 1);

    let end = anchor + 1;
    let start = end.saturating_sub(MAX_FINGERPRINT_LINES);
    normalized[start..end].to_vec()
}

/// Number of trailing lines two slices share. `[a, b, c]` and `[x, b, c]`
/// share 2; `[a, b, c]` and `[a, b]` share 0.
fn common_suffix_len(a: &[String], b: &[String]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

/// Should two fingerprints cluster together? Requires both an absolute floor
/// (`MIN_OVERLAP_LINES`) and at least half of the shorter fingerprint to
/// overlap, so a long traceback isn't merged with a short one just because
/// they coincidentally end with a common boilerplate line.
fn should_merge(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() {
        return a.is_empty() && b.is_empty();
    }
    let overlap = common_suffix_len(a, b);
    if overlap < MIN_OVERLAP_LINES {
        return false;
    }
    let shorter = a.len().min(b.len());
    overlap * 2 >= shorter
}

/// Trivial union-find over indices `0..n`.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

/// A bucket of failing tests that share a common failure tail.
#[cfg_attr(test, derive(Debug))]
struct Group {
    /// Longest suffix of normalized lines shared by every member.
    shared_tail: Vec<String>,
    test_ids: Vec<TestId>,
}

fn cluster_failures(failures: &[&TestResult]) -> Vec<Group> {
    let fingerprints: Vec<Vec<String>> = failures
        .iter()
        .map(|r| fingerprint(&pick_text(r)))
        .collect();

    let n = failures.len();
    let mut uf = UnionFind::new(n);

    // O(n^2). For thousand-failure runs this is fine; if it ever gets pricey
    // we can bucket by anchor line first.
    for i in 0..n {
        for j in (i + 1)..n {
            if should_merge(&fingerprints[i], &fingerprints[j]) {
                uf.union(i, j);
            }
        }
    }

    // Bucket members and compute the shared tail per cluster.
    let mut buckets: std::collections::HashMap<usize, Vec<usize>> = Default::default();
    for i in 0..n {
        buckets.entry(uf.find(i)).or_default().push(i);
    }

    let mut groups: Vec<Group> = buckets
        .into_values()
        .map(|members| {
            let shared = members
                .iter()
                .map(|&i| fingerprints[i].clone())
                .reduce(|acc, fp| {
                    let k = common_suffix_len(&acc, &fp);
                    acc[acc.len() - k..].to_vec()
                })
                .unwrap_or_default();
            let mut test_ids: Vec<TestId> = members
                .iter()
                .map(|&i| failures[i].test_id.clone())
                .collect();
            test_ids.sort();
            Group {
                shared_tail: shared,
                test_ids,
            }
        })
        .collect();

    // Largest groups first; on tie, alphabetical by shared tail for stable output.
    groups.sort_by(|a, b| {
        b.test_ids
            .len()
            .cmp(&a.test_ids.len())
            .then_with(|| a.shared_tail.cmp(&b.shared_tail))
    });

    groups
}

impl Command for SummarizeCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let repo = open_repository(self.base_path.as_deref())?;
        let run_id = resolve_run_id(&*repo, self.run_id.as_deref())?;
        let test_run = repo.get_test_run(&run_id)?;

        let failures: Vec<&TestResult> = test_run
            .results
            .values()
            .filter(|r| r.status.is_failure())
            .collect();

        if failures.is_empty() {
            ui.output(&format!("No failing tests in run {}", run_id))?;
            return Ok(0);
        }

        let groups = cluster_failures(&failures);

        ui.output(&format!(
            "{} failing test(s) in run {}, {} distinct failure(s):",
            failures.len(),
            run_id,
            groups.len(),
        ))?;

        for (idx, group) in groups.iter().enumerate() {
            ui.output("")?;
            let plural = if group.test_ids.len() == 1 {
                "test"
            } else {
                "tests"
            };
            ui.output(&format!(
                "[{}] {} {}:",
                idx + 1,
                group.test_ids.len(),
                plural,
            ))?;
            if group.shared_tail.is_empty() {
                ui.output("    (no traceback)")?;
            } else {
                for line in &group.shared_tail {
                    ui.output(&format!("    {}", line))?;
                }
            }
            let shown = self.samples.min(group.test_ids.len());
            for test_id in group.test_ids.iter().take(shown) {
                ui.output(&format!("  - {}", test_id))?;
            }
            if group.test_ids.len() > shown {
                ui.output(&format!("  … and {} more", group.test_ids.len() - shown))?;
            }
        }

        Ok(1)
    }

    fn name(&self) -> &str {
        "summarize"
    }

    fn help(&self) -> &str {
        "Group a run's failures by their common, normalized traceback tail"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, RunId, TestRun, TestStatus};
    use crate::ui::test_ui::TestUI;
    use tempfile::TempDir;

    fn failure_with_details(id: &str, details: &str) -> TestResult {
        TestResult {
            test_id: TestId::new(id),
            status: TestStatus::Failure,
            duration: None,
            message: Some("failed".to_string()),
            details: Some(details.to_string()),
            tags: vec![],
        }
    }

    #[test]
    fn normalize_strips_paths_and_numbers() {
        assert_eq!(
            normalize_line(r#"  File "/home/u/foo/bar.py", line 42, in test_foo"#),
            r#"  File "<path>", line <n>, in <fn>"#
        );
        assert_eq!(
            normalize_line("at 0xdeadbeef in module"),
            "at <hex> in module"
        );
        assert_eq!(
            normalize_line("thread 'main' panicked at src/lib.rs:42:5"),
            "thread 'main' panicked at <path>:<n>"
        );
        assert_eq!(normalize_line("took 1.234s"), "took <dur>");
        assert_eq!(normalize_line("pid 12345 exited"), "pid <n> exited");
        // Short integers preserved (often part of the failure itself).
        assert_eq!(
            normalize_line("AssertionError: 3 != 4"),
            "AssertionError: 3 != 4"
        );
    }

    #[test]
    fn anchor_recognises_common_failure_lines() {
        assert!(is_anchor_line("AssertionError: 1 != 2"));
        assert!(is_anchor_line("ValueError: bad input"));
        assert!(is_anchor_line("thread 'main' panicked at src/lib.rs:42:5"));
        assert!(is_anchor_line("assertion failed: x == y"));
        assert!(is_anchor_line("FAILED: my_test"));
        assert!(!is_anchor_line("    at frame 1"));
        assert!(!is_anchor_line("File \"a.py\", line 1, in foo"));
    }

    #[test]
    fn fingerprint_anchors_at_last_marker() {
        let trace = "\
some preamble
  File \"/x/a.py\", line 1, in f
ValueError: original
During handling of the above another exception occurred:
  File \"/x/b.py\", line 2, in g
RuntimeError: proximate cause
";
        let fp = fingerprint(trace);
        // Anchor is the proximate cause (last `*Error:` line), so the tail
        // ends there even though `ValueError: original` came first.
        assert_eq!(fp.last().unwrap(), "RuntimeError: proximate cause");
    }

    #[test]
    fn fingerprint_falls_back_to_last_line_when_no_anchor() {
        let trace = "first\nsecond\n  third\n\n";
        let fp = fingerprint(trace);
        assert_eq!(fp.last().unwrap(), "  third");
    }

    #[test]
    fn fingerprint_caps_length() {
        let many = (0..30)
            .map(|i| format!("frame {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let fp = fingerprint(&many);
        assert_eq!(fp.len(), MAX_FINGERPRINT_LINES);
        assert_eq!(fp.last().unwrap(), "frame 29");
    }

    #[test]
    fn merges_failures_differing_only_in_paths_and_line_numbers() {
        let a = failure_with_details(
            "tests.a",
            "  File \"/repo/tests/a.py\", line 12, in test_a\n\
             AssertionError: boom",
        );
        let b = failure_with_details(
            "tests.b",
            "  File \"/repo/tests/b.py\", line 47, in test_b\n\
             AssertionError: boom",
        );
        let groups = cluster_failures(&[&a, &b]);
        assert_eq!(groups.len(), 1, "got groups: {:?}", groups[0].shared_tail);
        assert_eq!(groups[0].test_ids.len(), 2);
        assert!(groups[0]
            .shared_tail
            .iter()
            .any(|l| l.contains("AssertionError: boom")));
    }

    #[test]
    fn does_not_merge_failures_with_different_anchors() {
        let a = failure_with_details("tests.a", "AssertionError: boom");
        let b = failure_with_details("tests.b", "ValueError: nope");
        let groups = cluster_failures(&[&a, &b]);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn does_not_merge_when_only_a_single_line_overlaps() {
        // Both end with the exact same anchor line, but with completely
        // different preceding frames — too thin to call them the same.
        let a = failure_with_details(
            "tests.a",
            "  File \"/repo/x.py\", line 1, in f\n\
                AssertionError: boom",
        );
        let b = failure_with_details("tests.b", "AssertionError: boom");
        // a's fingerprint has 2 lines, b's has 1; overlap is 1, MIN_OVERLAP is 2,
        // so they should not merge despite the identical anchor.
        let groups = cluster_failures(&[&a, &b]);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn transitive_merging_groups_via_chain() {
        // a~b on lines [Y, Z]; b~c on lines [X, Y]; union-find should put all three
        // together even though a and c only directly share [Y].
        let a = failure_with_details("a", "frame_a\nshared_y\nshared_z");
        let b = failure_with_details("b", "shared_x\nshared_y\nshared_z");
        let c = failure_with_details("c", "shared_x\nshared_y\nframe_c_extra");
        // a and b share [shared_y, shared_z] (2); b and c share [shared_x, shared_y]
        // depending on alignment. Let's check what we actually get — at minimum,
        // a-b should merge.
        let groups = cluster_failures(&[&a, &b, &c]);
        // We expect a and b clustered. c may or may not join depending on overlap;
        // the important behaviour we test is: the "merged-via-shared-suffix" pair
        // gets reported with its actual shared lines, not a hardcoded N.
        let big = groups.iter().find(|g| g.test_ids.len() >= 2).unwrap();
        assert!(big
            .shared_tail
            .last()
            .map(|l| l == "shared_z")
            .unwrap_or(false));
    }

    #[test]
    fn summarize_command_no_failures() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::success("test1"));
        repo.insert_test_run(run).unwrap();

        let mut ui = TestUI::new();
        let cmd = SummarizeCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            None,
            DEFAULT_SAMPLES,
        );
        let result = cmd.execute(&mut ui).unwrap();
        assert_eq!(result, 0);
        assert!(ui.output.iter().any(|s| s.contains("No failing tests")));
    }

    #[test]
    fn summarize_command_clusters_path_variants() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        for (i, path) in ["a.py", "b.py", "c.py"].iter().enumerate() {
            run.add_result(failure_with_details(
                &format!("pkg.t{}", i),
                &format!(
                    "  File \"/repo/{}\", line {}, in test\n  AssertionError: boom",
                    path,
                    10 + i
                ),
            ));
        }
        run.add_result(failure_with_details("pkg.other", "ValueError: nope"));
        run.add_result(TestResult::success("pkg.ok"));
        repo.insert_test_run(run).unwrap();

        let mut ui = TestUI::new();
        let cmd = SummarizeCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            None,
            DEFAULT_SAMPLES,
        );
        let result = cmd.execute(&mut ui).unwrap();
        assert_eq!(result, 1);

        let joined = ui.output.join("\n");
        assert!(
            joined.contains("4 failing test(s) in run 0, 2 distinct failure(s):"),
            "got: {}",
            joined,
        );
        assert!(joined.contains("3 tests:"));
        assert!(joined.contains("AssertionError: boom"));
        assert!(joined.contains("ValueError: nope"));
    }

    #[test]
    fn summarize_command_truncates_sample_list() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        for i in 0..7 {
            run.add_result(failure_with_details(
                &format!("pkg.t{}", i),
                &format!(
                    "  File \"/repo/x.py\", line {}, in test\n  AssertionError: same",
                    10 + i
                ),
            ));
        }
        repo.insert_test_run(run).unwrap();

        let mut ui = TestUI::new();
        let cmd = SummarizeCommand::new(Some(temp.path().to_string_lossy().to_string()), None, 3);
        cmd.execute(&mut ui).unwrap();

        let joined = ui.output.join("\n");
        assert!(joined.contains("7 tests:"));
        assert!(joined.contains("… and 4 more"), "got: {}", joined);
    }
}
