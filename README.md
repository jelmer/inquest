# Test Repository - Rust Port

## Overview

This is a Rust port of the Python [testrepository](https://github.com/testing-cabal/testrepository) tool. It provides a database of test results which can be used as part of developer workflow to ensure/check things like:

* No commits without having had a test failure, test fixed cycle.
* No commits without new tests being added.
* What tests have failed since the last commit (to run just a subset).
* What tests are currently failing and need work.

Test results are inserted using subunit (and thus anything that can output subunit or be converted into a subunit stream can be accepted).

**Key Features:**
- Full compatibility with the Python version's on-disk repository format
- Fast, native binary with no Python runtime required
- All core commands implemented
- Support for .testr.conf configuration files

## Installation

Build from source:

```sh
cargo build --release
```

The binary will be available at `target/release/testr`.

## Quick Start

Create a config file `.testr.conf`:

```ini
[DEFAULT]
test_command=cargo test $IDOPTION
test_id_option=--test $IDFILE
```

Create a repository:

```sh
testr init
```

Run tests and load results:

```sh
testr run
```

Query the repository:

```sh
testr stats
testr last
testr failing
testr slowest
```

Re-run only failing tests:

```sh
testr run --failing
```

List available tests:

```sh
testr list-tests
```

Delete a repository:

```sh
rm -rf .testrepository
```

## Commands

### `testr init`

Initialize a new test repository in the current directory. Creates a `.testrepository/` directory with the necessary structure.

### `testr run`

Execute tests using the command defined in `.testr.conf` and load the results into the repository.

Options:
- `--failing`: Run only the tests that failed in the last run

### `testr load`

Load test results from stdin in subunit format.

```sh
my-test-runner | testr load
```

### `testr last`

Show results from the most recent test run, including timestamp, counts, and list of failing tests.

### `testr failing`

Show only the failing tests from the last run. Exits with code 0 if no failures, 1 if there are failures.

### `testr stats`

Show repository statistics including total test runs, latest run details, and total tests executed.

### `testr slowest`

Show the slowest tests from the last run, sorted by duration.

Options:
- `-n, --count <N>`: Number of tests to show (default: 10)

### `testr list-tests`

List all available tests by querying the test command with the list option from configuration.

## Global Options

All commands support:
- `-C, --directory <PATH>`: Specify repository path (defaults to current directory)

## Configuration

The `.testr.conf` file uses INI format with a `[DEFAULT]` section. Key options:

- `test_command`: Command to run tests (required)
- `test_id_option`: Option format for running specific tests (e.g., `--test $IDFILE`)
- `test_list_option`: Option to list all available tests
- `group_regex`: Regex to extract test group from test ID

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
```

## Repository Format

The `.testrepository/` directory contains:

- `format`: File containing format version ("1")
- `next-stream`: Counter for the next run ID
- `0`, `1`, `2`, ...: Individual test run files in subunit v2 binary format

This format is **fully compatible** with the Python testrepository tool, allowing you to use both implementations interchangeably.

## Compatibility

This Rust port maintains full on-disk format compatibility with the Python version of testrepository. You can:

- Initialize a repository with the Rust version and use it with the Python version
- Initialize a repository with the Python version and use it with the Rust version
- Mix usage between both implementations

## Licensing

Test Repository is under BSD / Apache 2.0 licences. See the file COPYING in the source for details.

## Authors

- Robert Collins (original Python implementation)
- Jelmer Vernooĳ (Rust port)

## Links

- Original Python version: https://github.com/testing-cabal/testrepository
- Subunit: http://subunit.readthedocs.io/
