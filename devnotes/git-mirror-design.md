# Mirroring inquest runs into a side ref in git

## Goal

Optionally mirror each completed test run into a git ref so that a
repository that records test results in `.inquest/` can also publish those
results into git itself. With the mirror enabled, results travel with the
history (push, fetch, share), and standard git plumbing
(`git ls-tree`, `git cat-file`) is enough to inspect them — no inquest
install required to read the data back.

The mirror is **best-effort and one-way**. `.inquest/` remains the source
of truth; the git ref is a derived view written after a run completes.

## Storage layout

All mirrored data lives under one ref:

    refs/inquest

This is *not* a git-notes ref — `git notes` requires note bodies to be
blobs and we want a richer tree shape. We build the ref directly via
plumbing.

The ref points at a commit whose tree is keyed by the commit a run was
executed against, then by run id:

    <commit-sha>/<run-id>/subunit          blob — raw subunit v2 bytes
    <commit-sha>/<run-id>/metadata.json    blob — JSON metadata for the run
    <commit-sha>/<run-id>/stderr           blob — captured stderr (omitted when empty)

The `<commit-sha>` segment is the value of `RunMetadata.git_commit` for
the run (i.e. `git rev-parse HEAD` at the moment the run started). If
`git_commit` is `None`, the run is not mirrored.

Multiple runs on the same commit accumulate side-by-side as sibling
`<run-id>/` subtrees. Re-mirroring an existing `<commit, run_id>` pair
replaces just that subtree; entries for other runs and other commits are
preserved.

The ref's commits form a normal commit chain (one new commit per
mirrored run, with the previous tip as parent). The chain is bookkeeping;
the only tree of interest is the tip's. Keeping the chain (rather than
collapsing) gives `git log refs/inquest` a useful audit trail and lets
`git push --force-with-lease` keep concurrent publishers honest.

### Why one blob per piece per run

- **One subunit blob per run** so the byte stream has a stable git object
  id. A consumer that wants to cite "the result of run 7 against commit
  `abcdef`" can record the blob OID directly and that OID never changes.
- **One metadata blob per run** so writing a new run is a strictly
  additive tree edit. There is no shared `metadata.json` to read-modify-
  write, no parsing-and-merging.
- **One stderr blob per run** for the same reasons. Stored uncompressed
  even though `.inquest/runs/<id>.stderr.gz` is gzipped on disk — git
  packfiles already deflate blobs, double-compressing buys nothing and
  makes `git cat-file -p <oid>` produce binary slop.

### `metadata.json` schema

```json
{
  "format": 1,
  "run_id": "7",
  "timestamp": "2026-04-29T10:11:12Z",
  "command": "cargo test --workspace",
  "concurrency": 8,
  "duration_secs": 142.3,
  "exit_code": 0,
  "test_args": ["--", "--nocapture"],
  "profile": "ci",
  "git_dirty": false,
  "totals": {
    "total": 412,
    "failures": 0,
    "errors": 0,
    "skips": 3
  }
}
```

All fields except `format` and `run_id` are optional. Unknown fields
must be ignored by readers. `format` is bumped only on incompatible
changes; new fields land behind `skip_serializing_if = "Option::is_none"`
so older readers keep working.

## Config

```toml
mirror_to_git_notes = true
```

The historical name has stuck even after dropping `git notes`. Default
unset → off. Standard profile overlay applies.

## Mirroring algorithm

For one completed run:

1. **Probe** — `git rev-parse --git-dir`. If we're not in a git repo, no-op
   at `info!`.
2. **Skip when no anchor commit** — if `metadata.git_commit` is `None`, no-op
   at `debug!`.
3. **Hash the data blobs** — read subunit, metadata JSON, and (if non-empty)
   captured stderr; pipe each through `git hash-object -w --stdin` to
   produce three OIDs.
4. **Read the existing tree** for `refs/inquest`, if any. Otherwise start
   with an empty root tree.
5. **Splice** — replace the subtree at `<commit>/<run_id>/` with one
   containing `subunit`, `metadata.json`, and `stderr` (the latter only
   when present). Through a temporary index:
   - `GIT_INDEX_FILE=tmp git read-tree <existing-root>` (skip when none).
   - `GIT_INDEX_FILE=tmp git rm --cached -r --quiet --ignore-unmatch -- <commit>/<run_id>`
     drops any prior mirror of this exact `(commit, run_id)` so vestigial
     files (e.g. an old `stderr`) don't survive.
   - For each new entry: `GIT_INDEX_FILE=tmp git update-index --add --cacheinfo 100644,<oid>,<commit>/<run_id>/<file>`.
   - `GIT_INDEX_FILE=tmp git write-tree` → new root OID.
6. **Commit** — `git commit-tree <root> [-p <prev-tip>] -m "..."` with a
   fixed inquest committer identity.
7. **Update the ref** — `git update-ref refs/inquest <new> <expected-prev>`.
   The expected-OID gates concurrent writers: a racing mirror loses the
   race and we log a warning.

All errors during mirroring are turned into `tracing::warn!` by the
caller; mirroring never fails a run.

## Git tooling

Pure shell-out via `std::process::Command`, consistent with the rest of
inquest's git interaction. The full command set:

- `git rev-parse --git-dir`
- `git rev-parse --verify --quiet refs/inquest`
- `git hash-object -w --stdin`
- `git read-tree <oid>` (under a per-call `GIT_INDEX_FILE`)
- `git update-index --add --cacheinfo / --remove`
- `git write-tree`
- `git commit-tree`
- `git update-ref`
- `git ls-tree` for reads

`gix` / `git2` would be cleaner for high-volume publishers and is a
viable future swap; the module is the only place that would need to
change.

## Failure handling

| Condition                                              | Behaviour                                  |
|--------------------------------------------------------|--------------------------------------------|
| Not inside a git repository                            | no-op, `info!`                             |
| `git` not on `PATH`                                    | no-op, `info!`                             |
| `metadata.git_commit` is `None`                        | no-op, `debug!`                            |
| Captured stderr is empty                               | omit the `stderr` blob; mirror metadata + subunit |
| `git hash-object` / `update-index` / `write-tree` fails | `warn!`, run still succeeds                |
| `git update-ref` fails (concurrent writer)             | `warn!`, run still succeeds, mirror is lost |
| Existing tree at `<commit>/<run>/` has unexpected shape | overwrite — newer mirror wins              |

## Inspecting from the CLI

```sh
# What commits have mirrored runs?
git ls-tree refs/inquest

# What runs ran against this commit?
git ls-tree refs/inquest:<commit>

# Inspect run 7's metadata against <commit>:
git cat-file -p refs/inquest:<commit>/7/metadata.json

# Pipe the subunit stream into a consumer:
git cat-file -p refs/inquest:<commit>/7/subunit | subunit-stats

# Read captured stderr if any:
git cat-file -p refs/inquest:<commit>/7/stderr
```

Path autocompletion under `git cat-file` doesn't follow refs, so
`refs/inquest:<commit>/<TAB>` doesn't expand — `git ls-tree` first.

## Sharing via remotes

`refs/inquest` is not under `refs/heads/` or `refs/tags/`, so it is not
pushed or fetched by default. Inquest does not push automatically; that
is a deliberate user/CI policy choice.

To publish:

```sh
git push origin 'refs/inquest:refs/inquest'
```

To subscribe (one-time per clone):

```sh
git config --add remote.origin.fetch '+refs/inquest:refs/inquest'
git fetch origin
```

GitHub stores and serves the ref but does not display it in the web UI
and applies no branch-protection rules to it. It is effectively
write-anything-with-push-access.

### Concurrent publishers

`update-ref` is a fast-forward with an expected-OID check, so concurrent
writers serialise: the loser sees a conflict and (in inquest's case)
warns and gives up. A CI setup with multiple machines pushing to the
same remote needs either:

- A single publisher (one CI job dedicated to publishing), or
- Retry-with-rebase on the publisher: re-read the remote tip, splice
  the new run on top, push again. We don't implement this today.

A separate concern: run ids are sequential per local `.inquest/`, so
two CI workers will both produce run 0 and clobber one another's data
under the same commit. If push-from-many-publishers becomes a real
workflow, run ids likely need a per-publisher disambiguator (CI job id,
hostname, UUID prefix). Out of scope for the initial mirror.

## Storage cost

Each run on each commit adds one subunit blob, one metadata blob, and
optionally one stderr blob. Subunit is binary but compresses well in
packfiles; identical blobs across runs deduplicate to a single OID
automatically. A repo with thousands of runs is fine; a repo with
millions of runs starts to hit GitHub's soft 5 GB recommended limit.
Pruning is out of scope for this iteration — see open questions.

## Out of scope (deliberately)

- Reading runs *from* the mirror back into `.inquest/`. Inquest never
  reads the ref; `.inquest/` remains the source of truth.
- Auto-pushing on mirror. The user/CI decides when to push.
- Pruning the ref when corresponding `.inquest/` runs are pruned. The
  mirror's per-commit subtree may still be useful even if the local
  data is gone.
- Backfilling existing local runs into the mirror. Easy follow-up
  (`inq mirror --backfill`).
- Per-publisher run-id disambiguation for shared remotes (see above).
- Switching from shell-out to `gix`/`git2`. Local optimisation, no
  externally-visible change.
