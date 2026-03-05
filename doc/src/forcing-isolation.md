# Forcing isolation

Sometimes it is useful to force a separate test runner instance for each test
executed. The `--isolated` flag will cause inq to execute a separate runner
per test

```sh
  $ inq run --isolated
```

In this mode inq first determines tests to run (either automatically listed,
using the failing set, or a user supplied load-list), and then spawns one test
runner per test it runs. To avoid cross-test-runner interactions concurrency
is disabled in this mode. `--analyze-isolation` supercedes `--isolated` if
they are both supplied.
