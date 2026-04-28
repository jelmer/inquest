# Hiding tests

Some test runners (for instance, `zope.testrunner`) report pseudo tests having to
do with bringing up the test environment rather than being actual tests that
can be executed. These are only relevant to a test run when they fail - the
rest of the time they tend to be confusing. For instance, the same 'test' may
show up on multiple parallel test runs, which will inflate the 'executed tests'
count depending on the number of worker threads that were used. Scheduling such
'tests' to run is also a bit pointless, as they are only ever executed
implicitly when preparing (or finishing with) a test environment to run other
tests in.

inq can restrict counts and reports to a particular tag selection using the
`filter_tags` configuration option (a space-separated list). Each entry is
either a positive tag (only results carrying that tag are counted) or a
negation prefixed with `!` (results carrying that tag are skipped).

The same selection can be supplied on the command line via `--tag`, which can
be repeated and overrides `filter_tags` from the config:

```sh
inq run --tag worker-0 --tag '!slow'
```
