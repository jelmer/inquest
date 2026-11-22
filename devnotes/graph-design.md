# Test Dependency Graph in `inquest`

## Overview

This document describes how `inquest` builds and maintains a graph of relationships
between tests over time, and how subcommands use that graph to provide fast health
checks and root-cause triage.

The graph captures two complementary things:

- **Coverage overlap**: which tests exercise the same code, derived from instrumentation
- **Failure co-occurrence**: which tests tend to fail together, derived empirically from
  accumulated run history

These are kept as separate signals and combined when querying, so the graph remains
useful even before enough failures have accumulated to make co-occurrence meaningful.

---

## Data Model

### Storage

The storage layer is split into two tiers:

- **Raw run records** — one JSONL file per run, written append-only at the end of each
  run. No locking required; concurrent runs write to separate files.
- **Derived graph** — a set of Parquet files, recomputed in batch from the raw run
  records on a configurable schedule or on demand. Reads are cheap and lock-free;
  writes replace files atomically.

Asserted edges are stored in `.inquest.toml` under `[graph.assertions]`, since they are
small, human-editable, and version-controllable alongside the project.

#### Run Record Format (JSONL, one file per run)

Each run is stored as `.inquest/runs/<run-id>.jsonl`. Every line is a JSON object.
The first line is the run header; subsequent lines are per-test result records.

```jsonc
// Line 1: run header
{
  "type": "run",
  "id": "a3f9c1",
  "timestamp": 1741862400,
  "commit": "d4e5f6a",
  "branch": "main",
  "dirty": false,
  "hostname": "builder-03",
  "concurrency": 8,
  "run_kind": "ci"
}

// Lines 2..N: per-test results
{
  "type": "result",
  "run_id": "a3f9c1",
  "identity": "tests::db::connection_pool::test_checkout_timeout",
  "status": "failed",           // "passed" | "failed" | "skipped" | "inferred-skipped"
  "duration_ms": 312,
  "retry_count": 0
}
```

Coverage data, when present (`--with-coverage`), is appended as additional lines:

```jsonc
{
  "type": "coverage",
  "run_id": "a3f9c1",
  "identity": "tests::db::connection_pool::test_checkout_timeout",
  "bloom": "<base64-encoded bloom filter>",
  "file_count": 14
}
```

#### Derived Graph Files (Parquet, under `.inquest/graph/`)

All Parquet files are rewritten atomically (write to a temp file, then rename) during
`inq graph rebuild`. They are read-only from the perspective of all other subcommands.

**`tests.parquet`** — one row per known test identity

| Column | Type | Description |
|---|---|---|
| `identity` | `utf8` | `{suite}::{test_name}`, primary key |
| `first_seen_at` | `int64` | unix timestamp |
| `last_seen_at` | `int64` | unix timestamp |
| `stale` | `bool` | true if unseen for more than `stale_after_n_runs` |

**`co_occurrence.parquet`** — pairwise counters aggregated over all runs

| Column | Type | Description |
|---|---|---|
| `test_a` | `utf8` | identity, lexicographically ≤ `test_b` |
| `test_b` | `utf8` | identity |
| `runs_both_ran` | `int32` | |
| `runs_both_failed` | `int32` | |
| `runs_a_failed_b_passed` | `int32` | |
| `runs_a_passed_b_failed` | `int32` | |
| `last_updated_at` | `int64` | unix timestamp of most recent contributing run |

**`coverage.parquet`** — most recent coverage bloom filter per test

| Column | Type | Description |
|---|---|---|
| `identity` | `utf8` | |
| `run_id` | `utf8` | run this coverage was recorded in |
| `bloom` | `binary` | bloom filter over source file paths |
| `file_count` | `int32` | approximate number of files covered |

**`graph_edges.parquet`** — derived directed edges

| Column | Type | Description |
|---|---|---|
| `from_test` | `utf8` | identity of the predictor test |
| `to_test` | `utf8` | identity of the predicted test |
| `p_b_given_a` | `float32` | P(B fails \| A fails) |
| `p_b_given_not_a` | `float32` | P(B fails \| A passes) |
| `lift` | `float32` | `p_b_given_a / p_b_given_not_a` |
| `coverage_overlap` | `float32` | Jaccard similarity of bloom filters, or null |
| `edge_weight` | `float32` | combined score used for ranking |
| `sample_count` | `int32` | number of runs this is based on |
| `computed_at` | `int64` | unix timestamp of last rebuild |

Manually asserted edges from `.inquest.toml` are merged in at query time, with a fixed
high weight, rather than being stored in the Parquet file. This keeps the file purely
derived and always safely regenerable.

### Edge Weight Formula

The `edge_weight` in `graph_edges.parquet` combines empirical and structural signals:

```
lift = P(B fails | A fails) / P(B fails | A passes)

coverage_component = coverage_overlap ?? 0.0   -- 0 if not available

edge_weight = (0.7 * lift_score) + (0.3 * coverage_component)
```

where `lift_score` is a dampened lift that penalises low sample counts:

```
confidence = min(sample_count / MIN_SAMPLES, 1.0)   -- MIN_SAMPLES = 20
lift_score = confidence * lift + (1 - confidence) * 1.0
```

This means edges with fewer than 20 samples are pulled toward neutral (lift=1.0),
avoiding false strong edges from sparse data.

---

## Graph Construction

### After Each Run

At the end of each run, `inq` writes a single JSONL file to `.inquest/runs/`. This is
the only write that happens at run time — it requires no locking and is safe for
concurrent runs. The graph Parquet files are not touched.

```
.inquest/runs/
  a3f9c1.jsonl
  b7d2e4.jsonl
  ...
```

### Periodic Edge Recomputation

Parquet files are recomputed on a configurable schedule (default: after every 10 new
runs, or when explicitly triggered with `inq graph rebuild`). The recomputation:

1. Scans all `.jsonl` files in `.inquest/runs/` that are newer than the last rebuild
2. Aggregates co-occurrence counts across all runs, applying the decay function to
   older runs (see Decay, below)
3. Computes `lift`, `p_b_given_a`, and `p_b_given_not_a` for each pair
4. Filters to pairs where `lift > LIFT_THRESHOLD` (default 2.0) and
   `p_b_given_a > MIN_CONDITIONAL_PROB` (default 0.3)
5. Writes directed edges — both directions if warranted, or just the stronger direction
6. Atomically replaces all Parquet files via rename

The recomputation is idempotent and safe to interrupt — the old Parquet files remain
valid until the rename step completes.

### Coverage Integration

If coverage data is present (from `--with-coverage` runs), bloom filters are compared
using estimated Jaccard similarity and stored as `coverage_overlap` in `graph_edges.parquet`.
This is used both for the edge weight formula and for bootstrapping the graph before
enough failures have accumulated.

When coverage is available but co-occurrence is sparse, edges are seeded from coverage
overlap alone, with a reduced weight. This lets the graph provide useful triage
suggestions from day one.

---

## Graph Maintenance

### Test Identity and Renaming

Test identities are matched by their `identity` string. If a test is renamed, `inq`
detects this (via a `--rename-from` flag on the CLI, or by matching coverage fingerprints)
and rewrites the affected JSONL records and Parquet files to use the new identity,
preserving co-occurrence history.

If a test disappears for more than N runs (default 50), it is marked `stale = true` in
`tests.parquet` but not removed, so history is preserved if the test reappears.

### Decay

During recomputation, run records older than a configurable window are weighted down to
account for codebase evolution causing old co-occurrences to no longer be meaningful.

```
age_factor = exp(-days_since_run / DECAY_HALFLIFE)   -- DECAY_HALFLIFE = 90 days
effective_count = actual_count * age_factor
```

This is applied per-run-record during aggregation, so the raw JSONL files are never
modified.

---

## Subcommand Designs

### `inq run` — Ordered Execution

Before running tests, `inq run` loads `graph_edges.parquet` and `coverage.parquet` into
memory to determine execution order.

**Ordering strategy:**

1. Compute a *coverage value score* per test: how many distinct modules does this test
   cover that have low coverage from faster tests? (derived from bloom filter Jaccard
   distances and historical duration)
2. Rank by `coverage_value / mean_duration` descending — cheap, broad-coverage tests first
3. Within ties, prefer tests with a higher historical failure rate (more likely to catch
   something)

This ordering is computed once per run and stored in the run header, so later analyses
can attribute any "fast failure detection" improvements to it.

**Early exit (optional, with `--fail-fast`):**

Once a failure is detected, `inq run` can consult the graph to skip tests that are
highly likely to fail as a consequence (high `p_b_given_a` from known failures), and
report them as `inferred-skipped` rather than running them. This reduces noise in the
initial failure report. Tests in this state are clearly labelled and can be explicitly
run with `inq run --include-inferred`.

---

### `inq triage` — Root Cause Identification

Given a set of failing tests (either from a specific run ID or passed on stdin),
`inq triage` loads `graph_edges.parquet` into memory and walks the graph to identify
the most likely root causes.

**Algorithm:**

1. Load the subgraph of all edges where both endpoints are in the failing set
2. Merge in any asserted edges from `.inquest.toml`
3. Find tests that have **no predecessor in the failing set** — i.e. tests that are
   failing but whose failure is not explained by another failing test in the graph
4. These are the *root candidates*
5. Rank root candidates by: how many other failing tests they explain (out-degree into
   the failing set), weighted by edge weight
6. Report them in ranked order, with the transitive set of tests each one explains

**Output format:**

```
$ inq triage --run last

Root cause candidates (3 of 14 failing tests are unexplained):

  1. tests::db::connection_pool::test_checkout_timeout    [explains 9 others]
       → tests::api::users::test_create_user
       → tests::api::users::test_list_users
       → tests::integration::auth::test_login
       ...

  2. tests::cache::test_eviction_under_pressure           [explains 4 others]
       → tests::api::search::test_faceted_query
       ...

  3. tests::config::test_parse_tls_cert                   [explains 1 other]
       → tests::integration::tls::test_mutual_auth

Unexplained by graph (no known predecessors):
  • tests::db::connection_pool::test_checkout_timeout  (root candidate #1)
  • tests::cache::test_eviction_under_pressure         (root candidate #2)
  • tests::config::test_parse_tls_cert                 (root candidate #3)
```

**Disambiguation:**

If the graph has sparse data for the failing tests, `inq triage` says so explicitly
and falls back to coverage overlap as a proxy for relatedness.

---

### `inq canary` — Minimal Health-Check Set

`inq canary` computes or runs the smallest set of tests that gives broad coverage of
the codebase, optimised for fast overall health checks.

**Computing the canary set:**

1. Load all coverage bloom filters from `coverage.parquet`
2. Run a greedy set cover: iteratively pick the test that covers the most modules not
   yet covered by previously selected tests
3. Apply a cost function: prefer faster tests, break ties by historical failure rate
4. Stop when coverage breadth exceeds a threshold (default: 90% of modules touched by
   the full suite)

The canary set is stored in `.inquest/canary.json` and reused until the codebase changes
significantly (detected by a shift in the module coverage fingerprint across runs).

**Running the canary set:**

```
$ inq canary run
Running canary set (23 of 847 tests, estimated 12s)...
✓ All canary tests passed. (11.4s)

$ inq canary run
Running canary set (23 of 847 tests, estimated 12s)...
✗ 2 canary tests failed. (4.1s — stopped after first failure)

  FAILED tests::db::connection_pool::test_checkout_timeout
  FAILED tests::cache::test_eviction_under_pressure

  Run `inq triage --run last` to identify root causes.
  Run `inq run` to execute the full suite.
```

**Canary drift detection:**

`inq canary status` reports whether the canary set is still representative:

```
$ inq canary status
Canary set: 23 tests, last updated 4 days ago
Coverage: 91.2% of modules (threshold: 90%)
Drift: 3 modules added since last update — consider running `inq canary update`
```

---

### `inq graph` — Graph Inspection and Management

```
$ inq graph show [TEST]
    Show the graph neighbourhood of a test, or the full graph summary.

$ inq graph rebuild
    Recompute all Parquet files from raw run JSONL records.

$ inq graph assert FROM TO [--reason TEXT]
    Manually assert a causal edge in .inquest.toml.

$ inq graph retract FROM TO
    Remove a manually asserted edge from .inquest.toml.

$ inq graph stats
    Summary statistics: number of nodes, edges, coverage, sparsity.

$ inq graph dot [--failing-run RUN_ID]
    Emit the graph (or a subgraph for a specific failing run) in Graphviz DOT format.
```

Example output of `inq graph show tests::db::connection_pool::test_checkout_timeout`:

```
tests::db::connection_pool::test_checkout_timeout
  Predecessors (tests that predict this one failing):
    none

  Successors (tests this predicts will fail):
    tests::api::users::test_create_user          lift=8.4  p=0.91  n=47
    tests::api::users::test_list_users           lift=7.2  p=0.89  n=47
    tests::integration::auth::test_login         lift=6.1  p=0.83  n=47
    tests::api::search::test_basic_query         lift=3.2  p=0.61  n=47

  Coverage overlap (top 3):
    tests::db::connection_pool::test_idle_timeout   jaccard=0.84
    tests::db::connection_pool::test_max_size       jaccard=0.71

  First seen: 2025-11-03
  Last failed: 2026-03-10 (run a3f9c1)
```

---

## Configuration

All thresholds are configurable in `.inquest.toml`:

```toml
[graph]
rebuild_every_n_runs = 10
min_samples_for_edge = 20
lift_threshold = 2.0
min_conditional_prob = 0.3
decay_halflife_days = 90
stale_after_n_runs = 50

# Manually asserted edges. Merged at query time with a fixed high weight.
[[graph.assertions]]
from = "tests::db::connection_pool::test_checkout_timeout"
to = "tests::api::users::test_create_user"
reason = "API users test requires a working connection pool"
asserted_by = "jelmer"

[canary]
coverage_threshold = 0.90
max_canary_size = 50

[triage]
include_inferred_skips = false
min_edge_weight_for_triage = 0.5
```

---

## Directory Layout

A fully populated `.inquest/` directory looks like:

```
.inquest/
  runs/
    a3f9c1.jsonl       # one file per completed run
    b7d2e4.jsonl
    ...
  graph/
    tests.parquet
    co_occurrence.parquet
    coverage.parquet
    graph_edges.parquet
  canary.json          # current canary test set
```

The `runs/` directory is the authoritative source of truth. All files under `graph/`
are derived and can be regenerated at any time with `inq graph rebuild`.

---

## Bootstrapping and Cold Start

A fresh `inquest` installation has no run records. The recommended bootstrapping path is:

1. Run the full suite once with `--with-coverage` to write the first JSONL record
   including coverage data
2. `inq graph rebuild` computes initial Parquet files from coverage alone
3. `inq canary update` computes an initial canary set from the coverage Parquet
4. `inq graph assert` can be used to seed known relationships from code review knowledge
5. After ~20 failed runs, co-occurrence data becomes meaningful and the graph self-improves

Until `min_samples_for_edge` is reached, `inq triage` falls back to coverage similarity
and makes its data sparsity explicit in output, so users are not misled by low-confidence
edges.
