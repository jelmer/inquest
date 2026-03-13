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
