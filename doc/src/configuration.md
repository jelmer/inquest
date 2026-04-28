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

## Profiles

A TOML config may declare named **profiles** under `[profiles.<name>]` to
switch between alternative sets of values. Top-level fields form the
implicit *base* layer; a selected profile overlays its set fields on top
of the base, leaving unset fields alone.

```toml
test_command = "python -m subunit.run discover . $LISTOPT $IDOPTION"
test_id_option = "--load-list $IDFILE"
test_list_option = "--list"
test_timeout = "1m"

# Optional: applied when no --profile / INQ_PROFILE is given.
default_profile = "dev"

[profiles.ci]
test_timeout = "5m"
test_order = "alphabetical"
max_duration = "30m"

[profiles.nightly]
test_timeout = "10m"
filter_tags = ""                 # explicit empty clears base value
test_order = "frequent-failing-first"
```

Selection precedence (highest first):

1. `--profile NAME` on the command line
2. `INQ_PROFILE` environment variable
3. `default_profile` from the config file
4. base only (no overlay)

The reserved name `default` always means "base only" and may not appear
as a `[profiles.default]` table. Profile names must be non-empty and
must not contain `.`, `/`, or whitespace, or start with `_`.

A profile that sets a field to the empty string (e.g.
`filter_tags = ""`) **clears** the base value rather than inheriting it
— `Some("")` is treated as a real override, not as "unset".

`inq config --list-profiles` prints the defined profile names; `inq
config --profile NAME` prints the resolved view through that profile,
annotating each value with `[profile:NAME]` when it came from the
overlay and `[config]` when it came from the base layer. The active
profile name is also recorded in run metadata, so `inq info` can show
which profile produced a given run.

Profiles are not supported in the legacy `.testr.conf` (INI) format;
use `inq upgrade` to migrate to TOML first.
