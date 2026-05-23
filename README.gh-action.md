# Inquest on GitHub Actions

`inq ci` formats test results as GitHub Actions workflow commands when it
detects it's running inside a workflow. Failures show up as red annotations
on the changed lines of the PR diff, each failing test gets its own
foldable section in the workflow log, and a markdown summary with
pass/fail counts is rendered on the workflow run page.

## Using the `jelmer/inquest` action

The simplest way to wire inquest into a workflow is the bundled composite
action. It installs `inq` (prebuilt binary if available, `cargo install`
fallback), restores and saves the `.inquest` history cache, and runs
`inq ci`:

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: jelmer/inquest@main
```

Once `v0` is tagged, pin to `@v0` (or a specific release) instead of
`@main` for stability.

### Inputs

- `version` (default `latest`): inquest release to install, e.g. `0.1.5`.
- `args` (default empty): extra arguments forwarded to `inq ci`, e.g.
  `--retry=2` or `-j 4`.
- `working-directory` (default workspace root): where to run `inq ci` and
  where the `.inquest` cache lives.
- `cache` (default `true`): set to `false` to skip the history cache.
- `cache-key-prefix` (default `inquest`): override if multiple jobs in
  the same repo should keep separate histories.
- `install-cargo-fallback` (default `true`): set to `false` to fail when
  no prebuilt binary exists for the runner (useful when you want
  predictable cold-start times).

### Outputs

- `exit-code`: the exit code from `inq ci` (0 on success, non-zero on
  test failures). Lets follow-up steps branch on the result without
  losing the workflow's overall pass/fail signal.

## Manual setup

If you'd rather not depend on the action, install inquest yourself and
call `inq ci` directly:

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo install inquest
      - run: inq ci
```

`inq ci` defaults to `--format=auto`, which detects `GITHUB_ACTIONS=true`
in the environment and switches to the GitHub workflow-command format on
its own. Pass `--format=github` explicitly if you want to force it (e.g.
when piping output through another tool that strips the env var).

## Caching test history across runs

`inq` stores test history in a `.inquest/` directory. Persisting that
directory across CI runs unlocks the features that depend on history:
the `frequent-failing-first` ordering (so a known-bad test fails the run
fast), the slow-test warnings, the `auto` timeout, and ETA reporting.
The `jelmer/inquest` action does this for you; the manual equivalent is:

```yaml
- uses: actions/cache/restore@v4
  with:
    path: .inquest
    key: inquest-
    restore-keys: inquest-
- run: inq ci
- if: always()
  uses: actions/cache/save@v4
  with:
    path: .inquest
    key: inquest-${{ github.run_id }}-${{ github.run_attempt }}
```

Splitting restore and save (rather than a single `actions/cache@v4` step)
is what makes history accumulate. `actions/cache` only saves when the
primary key wasn't already a hit, so a constant key freezes after the
first run. With the split form, the restore uses a project-wide prefix
to pick up the latest cache from any prior run, and the save uses a
run-unique key so the updated `.inquest/` is always written back. If
your `.inquest/` directory lives somewhere other than the workspace
root, pass `-C <path>` to point `inq` at it.

## Tolerating flakes

Use `--retry=N` to re-run failing tests up to N times. A test that passes
on retry is reported as a `::warning::` annotation (still visible on the
PR diff) but does not fail the run. Tests that still fail after every
retry remain `::error::` annotations.

```yaml
- run: inq ci --retry=2
```

Retries are opt-in so the first run is always the honest signal; turn
them on when you actively want flake tolerance.

## Sharding across matrix jobs

Combine `inq ci` with `inq shard` to split a large suite across parallel
runners. Each shard produces its own annotations and summary section; if
you also restore the same cache key in each shard, ordering decisions
stay consistent across the matrix.

```yaml
strategy:
  matrix:
    shard: [1, 2, 3, 4]
steps:
  - uses: actions/checkout@v4
  - run: cargo install inquest
  - run: inq shard ${{ matrix.shard }}/4 > shard.txt
  - run: inq run --load-list shard.txt
```

## GitLab CI

`--format=gitlab` (or `--format=auto` when `GITLAB_CI=true` is set) emits
the same workflow-command wire format. GitLab renders the annotations in
its job log; the markdown summary is GitHub-specific and is skipped.
