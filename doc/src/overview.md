# Overview

Inquest is a small application for tracking test results. Any test run
that can be represented as a subunit stream can be inserted into a repository.

Typical workflow is to have a repository into which test runs are inserted, and
then to query the repository to find out about issues that need addressing.
inq can fully automate this, but lets start with the low level facilities,
using the sample subunit stream included with inq

```sh
  # Note that there is an inquest.toml already (or .testr.conf):
  ls inquest.toml
  # Create a store to manage test results in.
  $ inq init
  # add a test result (shows failures)
  $ inq load < examples/example-failing-subunit-stream
  # see the tracked failing tests again
  $ inq failing
  # fix things
  $ inq load < examples/example-passing-subunit-stream
  # Now there are no tracked failing tests
  $ inq failing
```

Most commands in inq have comprehensive online help, and the commands

```sh
  $ inq help [command]
  $ inq commands
```

Will be useful to explore the system.
