# Inquest

## Overview

Inquest provides a database of test results which can be used as part of developer workflow to ensure/check things like:

* No commits without having had a test failure, test fixed cycle.
* No commits without new tests being added.
* What tests have failed since the last commit (to run just a subset).
* What tests are currently failing and need work.

Test results are inserted using subunit (and thus anything that can output subunit or be converted into a subunit stream can be accepted).

Inquest started as a Rust port of the Python [testrepository](https://github.com/testing-cabal/testrepository) tool, originally written by Robert Collins. It has since grown its own features and on-disk format.

**Key Features:**
- Fast, native binary with no Python runtime required
- SQLite-backed repository with rich run metadata
- Timeout support (per-test, overall, no-output)
- Auto-detection of project type
- Optional backward compatibility with testrepository's on-disk format
- Configuration via `inquest.toml` (TOML) or `.testr.conf` (legacy INI)

## Installation

Build from source:

```sh
cargo build --release
```

The binary will be available at `target/release/inq`.

## Quick Start

Create a config file `inquest.toml`:

```toml
test_command = "cargo test $IDOPTION"
test_id_option = "--test $IDFILE"
```

Create a repository:

```sh
inq init
```

Run tests and load results:

```sh
inq run
```

Query the repository:

```sh
inq stats
inq last
inq failing
inq slowest
```

Re-run only failing tests:

```sh
inq run --failing
```

List available tests:

```sh
inq list-tests
```

Delete a repository:

```sh
rm -rf .inquest
```

## Commands

### `inq auto`

Auto-detect the project type and generate an `inquest.toml` configuration file.

### `inq init`

Initialize a new test repository in the current directory. Creates a `.inquest/` directory with a SQLite metadata database and runs directory.

### `inq run`

Execute tests using the command defined in `inquest.toml` and load the results into the repository.

Options:
- `--failing`: Run only the tests that failed in the last run
- `--partial`: Partial run mode (update failing tests additively)
- `--force-init`: Create repository if it doesn't exist
- `--auto`: Auto-detect project type and generate `inquest.toml` if missing
- `--load-list <FILE>`: Run only tests listed in the file (one per line)
- `-j, --parallel <N>`: Run tests in parallel across N workers
- `--until-failure`: Run tests repeatedly until they fail
- `--max-iterations <N>`: Maximum iterations for `--until-failure`
- `--isolated`: Run each test in a separate process
- `--subunit`: Show output as a subunit stream
- `--all-output`: Show output from all tests (not just failures)
- `--test-timeout <TIMEOUT>`: Per-test timeout (`5m`, `auto`, or `disabled`)
- `--max-duration <DURATION>`: Overall run timeout
- `--no-output-timeout <DURATION>`: Kill test process if no output for this duration
- `TESTFILTER`: Regex patterns to filter which tests to run
- `-- TESTARGS`: Additional arguments passed through to the test command

### `inq load`

Load test results from stdin in subunit format.

```sh
my-test-runner | inq load
```

Options:
- `--partial`: Partial run mode (update failing tests additively)
- `--force-init`: Create repository if it doesn't exist

### `inq last`

Show results from a test run, including timestamp, counts, and list of failing tests.

Options:
- `-r, --run <ID>`: Run ID to show (defaults to latest; supports negative indices like `-1`, `-2`)
- `--subunit`: Output results as a subunit stream
- `--no-output`: Don't show test output/tracebacks for failed tests

### `inq info`

Show detailed information about a test run, including git commit, command, duration, exit code, and concurrency.

Options:
- `-r, --run <ID>`: Run ID to show (defaults to latest; supports negative indices)

### `inq failing`

Show only the failing tests from the last run. Exits with code 0 if no failures, 1 if there are failures.

Options:
- `--list`: List test IDs only, one per line (for scripting)
- `--subunit`: Output results as a subunit stream

### `inq running`

Show currently in-progress test runs, including live test counts. Uses lock files to track active runs.

### `inq config`

Print the resolved effective configuration. Combines values from the config file
(`inquest.toml`, `.inquest.toml`, or `.testr.conf`), CLI overrides, and built-in
defaults. Each value is annotated with its source (`[config]`, `[cli]`, or
`[default]`), making it easy to see what `inq run` would actually use.

Options accept the same overrides as `inq run`: `--test-timeout`, `--max-duration`,
`--no-output-timeout`, `--order`, and `-j/--parallel`.

### `inq stats`

Show repository statistics including total test runs, latest run details, and total tests executed.

### `inq slowest`

Show the slowest tests from the last run, sorted by duration.

Options:
- `-n, --count <N>`: Number of tests to show (default: 10)
- `--all`: Show all tests (not just top N)

### `inq flaky`

Rank tests by flakiness across recorded runs. Flakiness is measured by
pass↔fail transitions in consecutive runs in which the test was recorded,
so chronically broken tests rank low and genuinely flapping tests rank high.

Options:
- `-n, --count <N>`: Number of tests to show (default: 10)
- `--all`: Show all candidate tests (not just top N)
- `--min-runs <N>`: Minimum runs a test must appear in to be ranked (default: 5)

Each row reports `flake%` (transitions / max(1, runs - 1)), `fail%`
(failures / runs), the raw transition count, and the number of recorded runs.

### `inq log <TESTPATTERN>`

Show logs and tracebacks for specific tests, matched by glob-style patterns.

Options:
- `-r, --run <ID>`: Run ID to show logs from (defaults to latest)

Example:
```sh
inq log 'test.module.TestCase.*'
```

### `inq list-tests`

List all available tests by querying the test command with the list option from configuration.

### `inq analyze-isolation <TEST>`

Analyze test isolation issues using bisection to find which tests cause a target test to fail when run together but pass in isolation.

This command:
1. Runs the target test in isolation to verify it passes alone
2. Runs all tests together to verify the failure reproduces
3. Uses binary search to find the minimal set of tests causing the failure
4. Reports which tests interact with the target test

Example:
```sh
inq analyze-isolation test.module.TestCase.test_flaky
```

### `inq upgrade`

Upgrade a legacy `.testrepository/` directory to the new `.inquest/` format. Only available when built with the `testr` feature.

### `inq completions <SHELL>`

Generate shell completions for the given shell. Supported shells: `bash`, `zsh`, `fish`, `elvish`, `powershell`.

To enable completions:

```sh
# Zsh (add to ~/.zshrc)
eval "$(inq completions zsh)"

# Bash (add to ~/.bashrc)
eval "$(inq completions bash)"

# Fish (add to ~/.config/fish/config.fish)
inq completions fish | source
```

## Global Options

All commands support:
- `-C, --directory <PATH>`: Specify repository path (defaults to current directory)

## Configuration

inq looks for configuration in the following files (in order of priority):
`inquest.toml`, `.inquest.toml`, `.testr.conf`. The TOML format is preferred;
`.testr.conf` (INI format with a `[DEFAULT]` section) is supported for backward
compatibility.

Key options:

- `test_command`: Command to run tests (required)
- `test_id_option`: Option format for running specific tests (e.g., `--test $IDFILE`)
- `test_list_option`: Option to list all available tests
- `test_id_list_default`: Default value for $IDLIST when no specific tests
- `group_regex`: Regex to group related tests together during parallel execution
- `test_run_concurrency`: Command to determine concurrency level (e.g., `nproc`)
- `filter_tags`: Tags to filter test results by (for parallel execution)
- `instance_provision`: Command to provision test instances (receives `$INSTANCE_COUNT`)
- `instance_execute`: Command template for running tests in an instance (receives `$INSTANCE_ID`)
- `instance_dispose`: Command to clean up test instances (receives `$INSTANCE_ID`)

### Variable Substitution

The following variables are available for use in `test_command`:

- `$IDOPTION`: Expands to the `test_id_option` with actual test IDs
- `$IDFILE`: Path to a temporary file containing test IDs (one per line)
- `$IDLIST`: Space-separated list of test IDs
- `$LISTOPT`: Expands to the `test_list_option`

### Example Configurations

#### Rust with Cargo

```toml
test_command = "cargo test $IDOPTION"
test_id_option = "--test $IDFILE"
test_list_option = "--list"
```

#### Python with pytest

```toml
test_command = "pytest $IDOPTION"
test_id_option = "--test-id-file=$IDFILE"
test_list_option = "--collect-only -q"
```

#### Go

Use the `gotest-run` wrapper (ships with python-subunit):

```toml
test_command = "gotest-run $LISTOPT $IDOPTION"
test_id_option = "--id-file $IDFILE"
test_list_option = "--list"
```

`inq auto` generates this automatically when it sees a `go.mod`. The
wrapper enumerates tests via `go test -json -list ...` for `--list`,
fans out one `go test -json -run <regex>` invocation per affected
package for `--id-file`, and otherwise runs the whole tree. Subtests
are created at runtime by `t.Run` and aren't statically discoverable,
so they're absent from listings — but executing them by ID works
(e.g. `inq run --failing` correctly re-runs `pkg::TestX/sub_one`).

#### Advanced Configuration with Parallel Execution

```toml
test_command = "cargo test --quiet $IDOPTION"
test_id_option = "$IDLIST"
test_list_option = "--list"

# Use system CPU count for parallel execution
test_run_concurrency = "nproc"

# Group tests by module (keeps related tests together)
group_regex = "^(.*)::[^:]+$"
```

## Repository Format

Inquest has its own on-disk format, stored in a `.inquest/` directory:

- `metadata.db`: SQLite database storing run metadata (git commit, command, duration, exit code, concurrency)
- `runs/`: Directory containing individual test run files in subunit v2 binary format
- `runs/N.lock`: Lock files for tracking in-progress test runs

When built with the `testr` feature, inquest can also read and write the legacy `.testrepository/` format used by the Python testrepository tool. Use `inq upgrade` to migrate.

## Licensing

Inquest is under BSD / Apache 2.0 licences. See the file COPYING in the source for details.

## History

Inquest started as a Rust port of the Python [testrepository](https://github.com/testing-cabal/testrepository) tool, originally written by Robert Collins. It has since diverged with its own features:

- **New on-disk format** (`.inquest/`) using SQLite for metadata, with richer run information (git commit, command, duration, exit code, concurrency)
- **Run lock files** and `inq running` command for tracking in-progress test runs
- **Auto-detection** of project type (`inq auto`)
- **Timeout support**: per-test timeouts, overall run timeouts, and no-output timeouts with automatic process killing
- **`inq log`** command for viewing logs/tracebacks of individual tests by glob pattern
- **`inq info`** command for detailed run metadata
- **Test filtering** by regex patterns on the command line
- **Pass-through arguments** to the underlying test command via `--`
- **MCP server** for integration with AI assistants (optional feature)

## Links

- Original Python version: https://github.com/testing-cabal/testrepository
- Subunit: http://subunit.readthedocs.io/
