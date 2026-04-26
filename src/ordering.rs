//! Test ordering strategies
//!
//! Determines the order in which tests are presented to the runner. Ordering
//! is applied after filtering and before partitioning, so it affects both the
//! sequence of tests in serial runs and the input that the partitioner
//! distributes across parallel workers.

use crate::error::{Error, Result};
use crate::repository::TestId;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::Duration;

/// Strategy for ordering tests before execution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TestOrder {
    /// Preserve the order produced by test discovery (no reordering).
    #[default]
    Discovery,

    /// Sort tests alphabetically by their full test ID.
    Alphabetical,

    /// Run tests that failed in the previous run first, then everything else
    /// in its original order. Useful for fast iteration on broken tests
    /// without losing coverage of the rest of the suite.
    FailingFirst,

    /// Interleave tests across as many distinct prefixes as possible. The
    /// first N tests will come from N different modules / packages, so an
    /// early failure surfaces a broad systemic issue rather than a single
    /// module's bad day.
    Spread,

    /// Pseudo-randomly shuffle tests. With a seed the order is deterministic;
    /// without one the seed is derived from the system clock at run time.
    Shuffle {
        /// Optional seed for reproducible shuffles.
        seed: Option<u64>,
    },

    /// Run the historically slowest tests first. Reduces tail latency when
    /// the runner is parallel and a single long test would otherwise hold
    /// up the whole run.
    SlowestFirst,
}

impl TestOrder {
    /// Render this ordering as the canonical CLI/config string.
    pub fn as_str(&self) -> String {
        match self {
            TestOrder::Discovery => "discovery".to_string(),
            TestOrder::Alphabetical => "alphabetical".to_string(),
            TestOrder::FailingFirst => "failing-first".to_string(),
            TestOrder::Spread => "spread".to_string(),
            TestOrder::Shuffle { seed: None } => "shuffle".to_string(),
            TestOrder::Shuffle { seed: Some(s) } => format!("shuffle:{}", s),
            TestOrder::SlowestFirst => "slowest-first".to_string(),
        }
    }
}

impl FromStr for TestOrder {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("shuffle") {
            return match rest {
                "" => Ok(TestOrder::Shuffle { seed: None }),
                _ => {
                    let seed_str = rest.strip_prefix(['=', ':']).ok_or_else(|| {
                        Error::Config(format!(
                            "invalid test order '{}': use 'shuffle' or 'shuffle:<seed>'",
                            trimmed
                        ))
                    })?;
                    let seed: u64 = seed_str.parse().map_err(|_| {
                        Error::Config(format!(
                            "invalid shuffle seed '{}': must be a non-negative integer",
                            seed_str
                        ))
                    })?;
                    Ok(TestOrder::Shuffle { seed: Some(seed) })
                }
            };
        }
        match lower.as_str() {
            "discovery" | "default" | "" => Ok(TestOrder::Discovery),
            "alphabetical" | "alpha" | "sorted" => Ok(TestOrder::Alphabetical),
            "failing-first" | "failing_first" | "failing" => Ok(TestOrder::FailingFirst),
            "spread" | "interleave" | "interleaved" => Ok(TestOrder::Spread),
            "slowest-first" | "slowest_first" | "slowest" => Ok(TestOrder::SlowestFirst),
            other => Err(Error::Config(format!(
                "unknown test order '{}': expected one of discovery, alphabetical, failing-first, spread, shuffle[:<seed>], slowest-first",
                other
            ))),
        }
    }
}

/// Inputs that the ordering strategies may need.
///
/// Held by reference so the caller keeps ownership of the underlying data.
pub struct OrderingContext<'a> {
    /// Tests known to have failed in the most recent run, used by
    /// [`TestOrder::FailingFirst`].
    pub failing_tests: &'a [TestId],
    /// Historical test durations, used by [`TestOrder::SlowestFirst`].
    pub historical_times: &'a HashMap<TestId, Duration>,
    /// Regex used to extract a "prefix" group from each test ID for
    /// [`TestOrder::Spread`]. When `None`, a heuristic is used instead.
    pub group_regex: Option<&'a str>,
}

/// Apply an ordering strategy to a list of tests.
pub fn apply_order(
    tests: Vec<TestId>,
    order: &TestOrder,
    ctx: &OrderingContext<'_>,
) -> Result<Vec<TestId>> {
    if tests.len() <= 1 {
        return Ok(tests);
    }
    match order {
        TestOrder::Discovery => Ok(tests),
        TestOrder::Alphabetical => Ok(sort_alphabetical(tests)),
        TestOrder::FailingFirst => Ok(failing_first(tests, ctx.failing_tests)),
        TestOrder::Spread => spread(tests, ctx.group_regex),
        TestOrder::Shuffle { seed } => Ok(shuffle(tests, *seed)),
        TestOrder::SlowestFirst => Ok(slowest_first(tests, ctx.historical_times)),
    }
}

fn sort_alphabetical(mut tests: Vec<TestId>) -> Vec<TestId> {
    tests.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    tests
}

fn failing_first(tests: Vec<TestId>, failing: &[TestId]) -> Vec<TestId> {
    if failing.is_empty() {
        return tests;
    }
    let failing_set: HashSet<&TestId> = failing.iter().collect();
    let mut head = Vec::new();
    let mut tail = Vec::new();
    for test in tests {
        if failing_set.contains(&test) {
            head.push(test);
        } else {
            tail.push(test);
        }
    }
    head.extend(tail);
    head
}

fn slowest_first(tests: Vec<TestId>, durations: &HashMap<TestId, Duration>) -> Vec<TestId> {
    let mut indexed: Vec<(usize, TestId)> = tests.into_iter().enumerate().collect();
    // Tests with unknown duration sort to the end; stable on original index
    // so the relative order of unknown-duration tests is preserved.
    indexed.sort_by(|(ia, a), (ib, b)| {
        let da = durations.get(a);
        let db = durations.get(b);
        match (da, db) {
            (Some(x), Some(y)) => y.cmp(x).then(ia.cmp(ib)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => ia.cmp(ib),
        }
    });
    indexed.into_iter().map(|(_, t)| t).collect()
}

fn spread(tests: Vec<TestId>, group_regex: Option<&str>) -> Result<Vec<TestId>> {
    let prefix_fn: Box<dyn Fn(&str) -> String> = match group_regex {
        Some(pat) => {
            let re = regex::Regex::new(pat)
                .map_err(|e| Error::Config(format!("invalid group_regex '{}': {}", pat, e)))?;
            Box::new(move |s: &str| extract_prefix_with_regex(&re, s))
        }
        None => Box::new(|s: &str| heuristic_prefix(s)),
    };

    // Preserve insertion order both for buckets and the tests within them so
    // the output is deterministic and reflects discovery order within a
    // prefix.
    let mut bucket_order: Vec<String> = Vec::new();
    let mut buckets: HashMap<String, Vec<TestId>> = HashMap::new();
    for test in tests {
        let prefix = prefix_fn(test.as_str());
        if !buckets.contains_key(&prefix) {
            bucket_order.push(prefix.clone());
        }
        buckets.entry(prefix).or_default().push(test);
    }

    let total: usize = buckets.values().map(|v| v.len()).sum();
    let mut result = Vec::with_capacity(total);
    let mut cursors: Vec<usize> = vec![0; bucket_order.len()];
    while result.len() < total {
        let mut took_any = false;
        for (i, prefix) in bucket_order.iter().enumerate() {
            let bucket = buckets.get(prefix).unwrap();
            if cursors[i] < bucket.len() {
                result.push(bucket[cursors[i]].clone());
                cursors[i] += 1;
                took_any = true;
            }
        }
        if !took_any {
            break;
        }
    }
    Ok(result)
}

fn extract_prefix_with_regex(re: &regex::Regex, test_str: &str) -> String {
    if let Some(caps) = re.captures(test_str) {
        if let Some(named) = caps.name("group") {
            return named.as_str().to_string();
        }
        if caps.len() > 1 {
            return caps.get(1).unwrap().as_str().to_string();
        }
        return caps.get(0).unwrap().as_str().to_string();
    }
    test_str.to_string()
}

/// Best-effort prefix extraction when no `group_regex` is configured.
/// Strips the last `::` or `.` separated segment so e.g.
/// `pkg.mod.TestCase.test_a` becomes `pkg.mod.TestCase`.
fn heuristic_prefix(test_str: &str) -> String {
    if let Some(idx) = test_str.rfind("::") {
        return test_str[..idx].to_string();
    }
    if let Some(idx) = test_str.rfind('.') {
        return test_str[..idx].to_string();
    }
    test_str.to_string()
}

fn shuffle(mut tests: Vec<TestId>, seed: Option<u64>) -> Vec<TestId> {
    let actual_seed = seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xDEAD_BEEF_CAFE_BABE)
    });
    let mut rng = SplitMix64::new(actual_seed);
    // Fisher-Yates
    for i in (1..tests.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        tests.swap(i, j);
    }
    tests
}

/// Tiny self-contained PRNG so we don't pull in `rand` as a dependency.
/// SplitMix64 is fast, has good statistical properties, and is the standard
/// seeding routine for xoshiro / xoroshiro families.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        // Avoid the all-zero state degenerate case.
        SplitMix64 {
            state: seed.wrapping_add(0x9E37_79B9_7F4A_7C15),
        }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(items: &[&str]) -> Vec<TestId> {
        items.iter().map(|s| TestId::new(*s)).collect()
    }

    #[test]
    fn parse_basic_variants() {
        assert_eq!(
            "default".parse::<TestOrder>().unwrap(),
            TestOrder::Discovery
        );
        assert_eq!("".parse::<TestOrder>().unwrap(), TestOrder::Discovery);
        assert_eq!(
            "alphabetical".parse::<TestOrder>().unwrap(),
            TestOrder::Alphabetical
        );
        assert_eq!(
            "alpha".parse::<TestOrder>().unwrap(),
            TestOrder::Alphabetical
        );
        assert_eq!(
            "failing-first".parse::<TestOrder>().unwrap(),
            TestOrder::FailingFirst
        );
        assert_eq!(
            "failing_first".parse::<TestOrder>().unwrap(),
            TestOrder::FailingFirst
        );
        assert_eq!("spread".parse::<TestOrder>().unwrap(), TestOrder::Spread);
        assert_eq!(
            "slowest-first".parse::<TestOrder>().unwrap(),
            TestOrder::SlowestFirst
        );
    }

    #[test]
    fn parse_shuffle() {
        assert_eq!(
            "shuffle".parse::<TestOrder>().unwrap(),
            TestOrder::Shuffle { seed: None }
        );
        assert_eq!(
            "shuffle:42".parse::<TestOrder>().unwrap(),
            TestOrder::Shuffle { seed: Some(42) }
        );
        assert_eq!(
            "shuffle=7".parse::<TestOrder>().unwrap(),
            TestOrder::Shuffle { seed: Some(7) }
        );
    }

    #[test]
    fn parse_case_insensitive_and_trimmed() {
        assert_eq!(
            "  ALPHABETICAL  ".parse::<TestOrder>().unwrap(),
            TestOrder::Alphabetical
        );
        assert_eq!(
            "Failing-First".parse::<TestOrder>().unwrap(),
            TestOrder::FailingFirst
        );
    }

    #[test]
    fn parse_unknown_rejected() {
        let err = "weird-order".parse::<TestOrder>().unwrap_err();
        assert!(err.to_string().contains("unknown test order"));
    }

    #[test]
    fn parse_bad_shuffle_seed() {
        assert!("shuffle:abc".parse::<TestOrder>().is_err());
        assert!("shuffle-7".parse::<TestOrder>().is_err());
    }

    #[test]
    fn as_str_round_trip() {
        for order in [
            TestOrder::Discovery,
            TestOrder::Alphabetical,
            TestOrder::FailingFirst,
            TestOrder::Spread,
            TestOrder::Shuffle { seed: None },
            TestOrder::Shuffle { seed: Some(123) },
            TestOrder::SlowestFirst,
        ] {
            let parsed: TestOrder = order.as_str().parse().unwrap();
            assert_eq!(parsed, order);
        }
    }

    fn empty_ctx() -> OrderingContext<'static> {
        // SAFETY: returning empty references with 'static lifetime is fine for
        // these shared empty values.
        static EMPTY_FAILING: Vec<TestId> = Vec::new();
        static EMPTY_TIMES: std::sync::OnceLock<HashMap<TestId, Duration>> =
            std::sync::OnceLock::new();
        OrderingContext {
            failing_tests: &EMPTY_FAILING,
            historical_times: EMPTY_TIMES.get_or_init(HashMap::new),
            group_regex: None,
        }
    }

    #[test]
    fn discovery_preserves_order() {
        let tests = ids(&["b", "a", "c"]);
        let result = apply_order(tests.clone(), &TestOrder::Discovery, &empty_ctx()).unwrap();
        assert_eq!(result, tests);
    }

    #[test]
    fn alphabetical_sorts() {
        let tests = ids(&["mod.b", "mod.a", "mod.c"]);
        let result = apply_order(tests, &TestOrder::Alphabetical, &empty_ctx()).unwrap();
        assert_eq!(result, ids(&["mod.a", "mod.b", "mod.c"]));
    }

    #[test]
    fn failing_first_promotes_failures() {
        let tests = ids(&["a", "b", "c", "d"]);
        let failing = ids(&["c", "a"]);
        let ctx = OrderingContext {
            failing_tests: &failing,
            historical_times: &HashMap::new(),
            group_regex: None,
        };
        let result = apply_order(tests, &TestOrder::FailingFirst, &ctx).unwrap();
        // Failures come first in their *input* order (a before c), then the rest.
        assert_eq!(result, ids(&["a", "c", "b", "d"]));
    }

    #[test]
    fn failing_first_with_no_failures_is_noop() {
        let tests = ids(&["a", "b", "c"]);
        let ctx = empty_ctx();
        let result = apply_order(tests.clone(), &TestOrder::FailingFirst, &ctx).unwrap();
        assert_eq!(result, tests);
    }

    #[test]
    fn slowest_first_orders_by_duration() {
        let tests = ids(&["fast", "slow", "medium", "unknown"]);
        let mut durations = HashMap::new();
        durations.insert(TestId::new("fast"), Duration::from_millis(10));
        durations.insert(TestId::new("slow"), Duration::from_secs(5));
        durations.insert(TestId::new("medium"), Duration::from_secs(1));
        let ctx = OrderingContext {
            failing_tests: &[],
            historical_times: &durations,
            group_regex: None,
        };
        let result = apply_order(tests, &TestOrder::SlowestFirst, &ctx).unwrap();
        assert_eq!(result, ids(&["slow", "medium", "fast", "unknown"]));
    }

    #[test]
    fn spread_interleaves_prefixes_heuristic() {
        // Default heuristic strips the last dot-separated segment.
        let tests = ids(&[
            "modA.test1",
            "modA.test2",
            "modA.test3",
            "modB.test1",
            "modB.test2",
            "modC.test1",
        ]);
        let result = apply_order(tests, &TestOrder::Spread, &empty_ctx()).unwrap();
        assert_eq!(
            result,
            ids(&[
                "modA.test1",
                "modB.test1",
                "modC.test1",
                "modA.test2",
                "modB.test2",
                "modA.test3",
            ])
        );
    }

    #[test]
    fn spread_uses_group_regex() {
        let tests = ids(&[
            "tests::alpha::one",
            "tests::alpha::two",
            "tests::beta::one",
            "tests::beta::two",
        ]);
        let ctx = OrderingContext {
            failing_tests: &[],
            historical_times: &HashMap::new(),
            group_regex: Some(r"^tests::(?P<group>\w+)::"),
        };
        let result = apply_order(tests, &TestOrder::Spread, &ctx).unwrap();
        assert_eq!(
            result,
            ids(&[
                "tests::alpha::one",
                "tests::beta::one",
                "tests::alpha::two",
                "tests::beta::two",
            ])
        );
    }

    #[test]
    fn spread_handles_double_colon_heuristic() {
        let tests = ids(&[
            "crate::mod_a::test_one",
            "crate::mod_a::test_two",
            "crate::mod_b::test_one",
        ]);
        let result = apply_order(tests, &TestOrder::Spread, &empty_ctx()).unwrap();
        assert_eq!(
            result,
            ids(&[
                "crate::mod_a::test_one",
                "crate::mod_b::test_one",
                "crate::mod_a::test_two",
            ])
        );
    }

    #[test]
    fn spread_with_invalid_regex_errors() {
        let tests = ids(&["a", "b"]);
        let ctx = OrderingContext {
            failing_tests: &[],
            historical_times: &HashMap::new(),
            group_regex: Some(r"^(unclosed"),
        };
        let err = apply_order(tests, &TestOrder::Spread, &ctx).unwrap_err();
        assert!(err.to_string().contains("invalid group_regex"));
    }

    #[test]
    fn shuffle_is_deterministic_with_seed() {
        let tests = ids(&["a", "b", "c", "d", "e", "f", "g"]);
        let order = TestOrder::Shuffle { seed: Some(42) };
        let r1 = apply_order(tests.clone(), &order, &empty_ctx()).unwrap();
        let r2 = apply_order(tests.clone(), &order, &empty_ctx()).unwrap();
        assert_eq!(r1, r2);
        // And it's actually a permutation of the input
        let mut sorted_in = tests;
        sorted_in.sort();
        let mut sorted_out = r1;
        sorted_out.sort();
        assert_eq!(sorted_in, sorted_out);
    }

    #[test]
    fn shuffle_changes_order() {
        // With a 7-element input and a seed, at least one position should differ
        // from the input. (SplitMix64 is well-distributed enough that this
        // assertion is safe.)
        let tests = ids(&["a", "b", "c", "d", "e", "f", "g"]);
        let order = TestOrder::Shuffle { seed: Some(1) };
        let result = apply_order(tests.clone(), &order, &empty_ctx()).unwrap();
        assert_ne!(result, tests);
    }

    #[test]
    fn empty_input_unchanged() {
        let result = apply_order(Vec::new(), &TestOrder::Alphabetical, &empty_ctx()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn single_test_unchanged_for_all_orders() {
        let tests = ids(&["only"]);
        for order in [
            TestOrder::Discovery,
            TestOrder::Alphabetical,
            TestOrder::FailingFirst,
            TestOrder::Spread,
            TestOrder::Shuffle { seed: Some(1) },
            TestOrder::SlowestFirst,
        ] {
            let result = apply_order(tests.clone(), &order, &empty_ctx()).unwrap();
            assert_eq!(result, tests);
        }
    }
}
