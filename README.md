# Inquest

## Overview

Inquest provides a database of test results which can be used as part of developer workflow to ensure/check things like:

* No commits without having had a test failure, test fixed cycle.
* No commits without new tests being added.
* What tests have failed since the last commit (to run just a subset).
* What tests are currently failing and need work.

Test results are inserted using subunit (and thus anything that can output subunit or be converted into a subunit stream can be accepted).

Inquest started as a Rust port of the Python [testrepository](https://github.com/testing-cabal/testrepository) tool, originally written by Robert Collins. It maintains full on-disk format compatibility with the original.

**Key Features:**
- Full compatibility with testrepository's on-disk repository format
- Fast, native binary with no Python runtime required
- All core commands implemented
- Support for .testr.conf configuration files

## Installation

Build from source:

```sh
cargo build --release
```

The binary will be available at `target/release/inq`.

## Quick Start

Create a config file `.testr.conf`:

```ini
[DEFAULT]
test_command=cargo test $IDOPTION
test_id_option=--test $IDFILE
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
rm -rf .testrepository
```

## Commands

### `inq init`

Initialize a new test repository in the current directory. Creates a `.testrepository/` directory with the necessary structure.

### `inq run`

Execute tests using the command defined in `.testr.conf` and load the results into the repository.

Options:
- `--failing`: Run only the tests that failed in the last run
- `--partial`: Partial run mode (update failing tests additively)
- `--force-init`: Create repository if it doesn't exist
- `--load-list <FILE>`: Run only tests listed in the file (one per line)
- `-j, --parallel <N>`: Run tests in parallel across N workers
- `--until-failure`: Run tests repeatedly until they fail
- `--isolated`: Run each test in a separate process

### `inq load`

Load test results from stdin in subunit format.

```sh
my-test-runner | inq load
```

Options:
- `--partial`: Partial run mode (update failing tests additively)
- `--force-init`: Create repository if it doesn't exist

### `inq last`

Show results from the most recent test run, including timestamp, counts, and list of failing tests.

Options:
- `--subunit`: Output results as a subunit stream

### `inq failing`

Show only the failing tests from the last run. Exits with code 0 if no failures, 1 if there are failures.

Options:
- `--list`: List test IDs only, one per line (for scripting)
- `--subunit`: Output results as a subunit stream

### `inq stats`

Show repository statistics including total test runs, latest run details, and total tests executed.

### `inq slowest`

Show the slowest tests from the last run, sorted by duration.

Options:
- `-n, --count <N>`: Number of tests to show (default: 10)
- `--all`: Show all tests (not just top N)

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

## Global Options

All commands support:
- `-C, --directory <PATH>`: Specify repository path (defaults to current directory)

## Configuration

The `.testr.conf` file uses INI format with a `[DEFAULT]` section. Key options:

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

```ini
[DEFAULT]
test_command=cargo test $IDOPTION
test_id_option=--test $IDFILE
test_list_option=--list
```

#### Python with pytest

```ini
[DEFAULT]
test_command=pytest $IDOPTION
test_id_option=--test-id-file=$IDFILE
test_list_option=--collect-only -q
```

#### Advanced Configuration with Parallel Execution

```ini
[DEFAULT]
test_command=cargo test --quiet $IDOPTION
test_id_option=$IDLIST
test_list_option=--list

# Use system CPU count for parallel execution
test_run_concurrency=nproc

# Group tests by module (keeps related tests together)
group_regex=^(.*)::[^:]+$
```

## Repository Format

The `.testrepository/` directory contains:

- `format`: File containing format version ("1")
- `next-stream`: Counter for the next run ID
- `0`, `1`, `2`, ...: Individual test run files in subunit v2 binary format

This format is **fully compatible** with the Python testrepository tool, allowing you to use both implementations interchangeably.

## Compatibility

Inquest maintains full on-disk format compatibility with the Python version of testrepository. You can:

- Initialize a repository with inquest and use it with testrepository
- Initialize a repository with testrepository and use it with inquest
- Mix usage between both implementations

## Licensing

Inquest is under BSD / Apache 2.0 licences. See the file COPYING in the source for details.

## History

Inquest started as a Rust port of the Python [testrepository](https://github.com/testing-cabal/testrepository) tool, originally written by Robert Collins.

## Links

- Original Python version: https://github.com/testing-cabal/testrepository
- Subunit: http://subunit.readthedocs.io/
