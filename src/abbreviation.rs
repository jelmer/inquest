//! Test name abbreviation expansion.
//!
//! Supports `brz selftest -s` style abbreviations where leading dotted
//! segments of a test name can be collapsed to their initial letters when
//! the expansion is unique against the known test list.
//!
//! For example, given a project whose only top-level segment beginning with
//! `b` is `breezy`, and whose only `breezy.t*` child segment is `tests`,
//! the abbreviation `bt.test_foo` expands to `breezy.tests.test_foo`.
//!
//! The expansion rule: each dotted piece of the abbreviation is matched
//! against the corresponding sequence of segments in the known test IDs.
//! A piece may either match a single segment literally, or — if it consists
//! of more than one character — be interpreted as one initial letter per
//! segment. The expansion is rejected if more than one distinct prefix
//! survives.
//!
//! Only the leading pieces of the abbreviation are expanded. The last piece
//! is treated as a tail filter (typically the test method or class name)
//! and is left untouched.

use crate::error::{Error, Result};

/// Expand a `brz selftest -s` style abbreviation into a concrete test ID prefix.
///
/// `abbrev` is the user-supplied abbreviation (e.g. `bt.test_foo`).
/// `test_ids` is the list of known full test IDs to match against.
///
/// Returns the expanded prefix string, or an error if the abbreviation
/// matches no tests or expands ambiguously.
pub fn expand_abbreviation(abbrev: &str, test_ids: &[&str]) -> Result<String> {
    if abbrev.is_empty() {
        return Err(Error::Config(
            "empty abbreviation cannot be expanded".to_string(),
        ));
    }

    let pieces: Vec<&str> = abbrev.split('.').collect();
    let (head, tail) = pieces.split_at(pieces.len().saturating_sub(1));
    let tail = tail.first().copied().unwrap_or("");

    // If there are no leading pieces (single piece input), match against
    // the first segment of each test ID. Treat the whole input as a head
    // piece so that `bt` (no dot) still expands.
    let (head, tail): (&[&str], &str) = if head.is_empty() {
        (pieces.as_slice(), "")
    } else {
        (head, tail)
    };

    // Build the set of unique segment paths from each test ID, restricted
    // to the depth implied by the abbreviation. We expand letter-by-letter
    // for each head piece, so the depth is the sum of the lengths of the
    // head pieces — except that a head piece that matches a segment
    // literally counts as one segment.
    let mut candidates: Vec<Vec<String>> = Vec::new();
    for test_id in test_ids {
        let segments: Vec<&str> = test_id.split('.').collect();
        if let Some(prefix) = match_pieces(head, &segments) {
            let owned: Vec<String> = prefix.into_iter().map(|s| s.to_string()).collect();
            if !candidates.iter().any(|p| p == &owned) {
                candidates.push(owned);
            }
        }
    }

    match candidates.len() {
        0 => Err(Error::Config(format!(
            "abbreviation '{}' matches no known tests",
            abbrev
        ))),
        1 => {
            let mut out = candidates.into_iter().next().unwrap().join(".");
            if !tail.is_empty() {
                out.push('.');
                out.push_str(tail);
            }
            Ok(out)
        }
        _ => {
            let mut shown: Vec<String> = candidates.iter().map(|p| p.join(".")).collect();
            shown.sort();
            shown.truncate(5);
            Err(Error::Config(format!(
                "abbreviation '{}' is ambiguous; could expand to: {}",
                abbrev,
                shown.join(", ")
            )))
        }
    }
}

/// Try to match a sequence of abbreviation pieces against a sequence of
/// segments. Returns the matched segment prefix on success.
///
/// Each piece may match either:
///   - a single segment, if the piece equals that segment literally, OR
///   - one initial letter per segment, if every character of the piece is
///     the first character of consecutive segments.
fn match_pieces<'a>(pieces: &[&str], segments: &'a [&'a str]) -> Option<Vec<&'a str>> {
    match_pieces_at(pieces, segments, 0)
}

fn match_pieces_at<'a>(
    pieces: &[&str],
    segments: &'a [&'a str],
    seg_idx: usize,
) -> Option<Vec<&'a str>> {
    let Some((piece, rest)) = pieces.split_first() else {
        return Some(Vec::new());
    };

    // Option A: literal match against a single segment.
    let mut literal_match: Option<Vec<&str>> = None;
    if seg_idx < segments.len() && segments[seg_idx] == *piece {
        if let Some(mut tail) = match_pieces_at(rest, segments, seg_idx + 1) {
            let mut out = Vec::with_capacity(tail.len() + 1);
            out.push(segments[seg_idx]);
            out.append(&mut tail);
            literal_match = Some(out);
        }
    }

    // Option B: each character is the initial letter of one segment.
    let mut letter_match: Option<Vec<&str>> = None;
    let chars: Vec<char> = piece.chars().collect();
    if !chars.is_empty() && seg_idx + chars.len() <= segments.len() {
        let mut ok = true;
        for (i, ch) in chars.iter().enumerate() {
            let seg = segments[seg_idx + i];
            if !seg.starts_with(*ch) {
                ok = false;
                break;
            }
        }
        if ok {
            if let Some(mut tail) = match_pieces_at(rest, segments, seg_idx + chars.len()) {
                let mut out = Vec::with_capacity(tail.len() + chars.len());
                for i in 0..chars.len() {
                    out.push(segments[seg_idx + i]);
                }
                out.append(&mut tail);
                letter_match = Some(out);
            }
        }
    }

    // Prefer the literal match: if a piece corresponds to a real segment
    // exactly, we treat that as the user's intent rather than as a
    // letter-by-letter abbreviation.
    literal_match.or(letter_match)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_single_letter_pair() {
        let tests = vec!["breezy.tests.test_foo", "breezy.tests.test_bar"];
        let got = expand_abbreviation("bt.test_foo", &tests).unwrap();
        assert_eq!(got, "breezy.tests.test_foo");
    }

    #[test]
    fn three_letter_prefix() {
        let tests = vec![
            "breezy.tests.bzr.test_x",
            "breezy.tests.git.test_y",
            "breezy.tests.test_top",
        ];
        let got = expand_abbreviation("btb.test_x", &tests).unwrap();
        assert_eq!(got, "breezy.tests.bzr.test_x");
    }

    #[test]
    fn literal_segment_passes_through() {
        let tests = vec!["breezy.tests.test_foo"];
        let got = expand_abbreviation("breezy.tests.test_foo", &tests).unwrap();
        assert_eq!(got, "breezy.tests.test_foo");
    }

    #[test]
    fn mixed_literal_and_abbrev() {
        let tests = vec!["breezy.tests.bzr.test_x"];
        let got = expand_abbreviation("breezy.tb.test_x", &tests).unwrap();
        assert_eq!(got, "breezy.tests.bzr.test_x");
    }

    #[test]
    fn no_tail_piece() {
        let tests = vec!["breezy.tests.test_foo", "breezy.tests.test_bar"];
        let got = expand_abbreviation("bt", &tests).unwrap();
        assert_eq!(got, "breezy.tests");
    }

    #[test]
    fn ambiguous_fails() {
        // Two distinct top-level segments both start with 'b', and each has
        // a child starting with 't', so 'bt' could mean either 'breezy.tests'
        // or 'boring.tools'.
        let tests = vec!["breezy.tests.x", "boring.tools.x"];
        let err = expand_abbreviation("bt.x", &tests).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ambiguous"), "{}", msg);
    }

    #[test]
    fn no_match_fails() {
        let tests = vec!["breezy.tests.test_foo"];
        let err = expand_abbreviation("zz.x", &tests).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no known tests"), "{}", msg);
    }

    #[test]
    fn empty_input_fails() {
        let tests = vec!["a.b.c"];
        let err = expand_abbreviation("", &tests).unwrap_err();
        assert!(err.to_string().contains("empty abbreviation"));
    }

    #[test]
    fn first_letter_collision_resolved_by_second() {
        // Two top-level segments start with 'b', but only one has a child
        // starting with 't', so 'bt' is unambiguous.
        let tests = vec![
            "breezy.tests.test_a",
            "breezy.tests.test_b",
            "boring.utils.helper",
        ];
        let got = expand_abbreviation("bt.test_a", &tests).unwrap();
        assert_eq!(got, "breezy.tests.test_a");
    }

    #[test]
    fn empty_segments_handled() {
        // Abbreviation longer than test path must not match.
        let tests = vec!["a.b"];
        let err = expand_abbreviation("abc.x", &tests).unwrap_err();
        assert!(err.to_string().contains("no known tests"));
    }
}
