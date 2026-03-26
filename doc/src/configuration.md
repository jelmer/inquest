# Configuration

inq is configured via a configuration file in the project directory.
The following file names are searched in order of priority:

1. `inquest.toml` (TOML format, preferred)
2. `.inquest.toml` (TOML format, hidden file)
3. `.testr.conf` (legacy INI format)

inq includes online help for all the options that can be set within it:

```sh
  $ inq help run
```

## Python

If your test suite is written in Python, the simplest - and usually correct
configuration is:

### TOML format (`inquest.toml`)

```toml
test_command = "python -m subunit.run discover . $LISTOPT $IDOPTION"
test_id_option = "--load-list $IDFILE"
test_list_option = "--list"
```

### Legacy INI format (`.testr.conf`)

```ini
    [DEFAULT]
    test_command=python -m subunit.run discover . $LISTOPT $IDOPTION
    test_id_option=--load-list $IDFILE
    test_list_option=--list
```

## Configuration reference

### Core settings

| Field | Description |
|---|---|
| `test_command` | Command to run tests. Supports `$LISTOPT`, `$IDOPTION`, `$IDFILE`, `$IDLIST` variables. |
| `test_list_option` | Argument to append when listing tests (replaces `$LISTOPT`). |
| `test_id_option` | Argument template for passing test IDs (replaces `$IDOPTION`). |
| `test_run_concurrency` | Shell command whose output sets the number of parallel workers. |
| `group_regex` | Regex for grouping tests onto the same worker (see [Grouping tests](./grouping-tests.md)). |

### Timeout settings

| Field | Description |
|---|---|
| `test_timeout` | Per-test timeout. `"disabled"` (default), `"auto"` (3x historical average), or a duration like `"5m"`. |
| `max_duration` | Overall run timeout. Same format as `test_timeout`. |
| `no_output_timeout` | Kill the runner if no output for this duration, e.g. `"120s"`. |

Durations are specified as a number with an optional unit suffix:
`s` (seconds, default), `m` (minutes), `h` (hours). Fractional values
like `"1.5m"` are supported.
