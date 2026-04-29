---
name: test-triage
description: Use when running, debugging, or triaging tests in any project inquest can manage — that includes projects with an existing inquest.toml, .testr.conf, or .inquest/ directory, AND any project `inq auto` can detect (Cargo/Rust, pytest, Python unittest, Go modules, Perl prove, Vitest, Jest). Prefer the inq_* MCP tools (inq_failure_summary, inq_test, inq_run, inq_failing, inq_slowest, inq_log) over shelling out to `inq`, `cargo test`, `pytest`, `go test`, or similar — they return structured JSON, run incrementally, and reuse the existing test repository instead of re-running everything.
---

# Triaging tests with inquest

The `inquest` MCP server is connected. Prefer its tools over shelling out
whenever the project is one inquest can manage — either it already has an
`inquest.toml` / `.testr.conf` / `.inquest/`, or it's a project type inquest
auto-detects (Cargo, pytest, Python unittest, Go modules, Perl prove, Vitest,
Jest). The `inq_*` tools handle detection themselves; you don't need to run
a setup step first.

## Quick decision table

| Goal | Tool |
| --- | --- |
| What's currently broken? | `inq_failing` (fast list) or `inq_failure_summary` (one-line message per failure) |
| Full traceback for one failure | `inq_test` with the test id from `inq_failure_summary` |
| Re-run only failing tests after a fix | `inq_run` with `failing_only: true` |
| Run a specific test or pattern | `inq_run` with `test_filters` |
| Run many specific tests at once | `inq_test_batch` |
| What changed between two runs? | `inq_diff` |
| Repository / latest-run overview | `inq_stats` or `inq_last` |
| List historical runs | `inq_list_runs` |
| Detect flaky tests | `inq_flaky` |
| Find slow tests | `inq_slowest` |
| Bisect an isolation problem | `inq_bisect` / `inq_analyze_isolation` |
| Search log output across a run | `inq_log` |

## Triage workflow

1. **Start with `inq_failure_summary`** — it gives a compact list of failing
   tests, each with a one-line message. This is almost always cheaper and more
   useful than `inq_log`.
2. **Drill in with `inq_test`** — pass a test id from the summary to get the
   full (truncated) traceback for that specific failure.
3. **Fix the code.**
4. **Verify with `inq_run` using `failing_only: true`** — reruns only the
   previously-failing tests instead of the whole suite.
5. **Once green, run the full suite** — `inq_run` with no filters — to confirm
   no regressions.

## Don't

- Don't run `inq` via `Bash` when an MCP equivalent exists. The MCP tools
  return structured data; the CLI prints progress bars and color codes.
- Don't run `cargo test` / `pytest` / etc. directly — those bypass the test
  repository, so subsequent `inq_*` queries won't see the run.
- Don't call `inq_log` for failure triage when `inq_failure_summary` would do.
  Reach for `inq_log` only when you need pattern-based search across a run.
