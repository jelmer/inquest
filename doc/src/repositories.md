# Repositories

An inq repository is a very simple disk structure. It contains the following
files (for a format 1 repository - the only current format):

* `format`: This file identifies the precise layout of the repository, in case future changes are needed.

* `next-stream`: This file contains the serial number to be used when adding another stream to the repository.

* `failing`: This file is a stream containing just the known failing tests.
  It is updated whenever a new stream is added to the repository, so that it only references known failing tests.

* `#N` - all the streams inserted in the repository are given a serial number.

* `metadata.tdb`: A TDB (Trivial Database) key-value store for run metadata. This is an inquest extension (not present in the Python testrepository). It stores:
  - `run:<id>` - JSON object with run metadata:
    - `git_commit` - the git commit SHA at the time of the run
    - `git_dirty` - whether the working tree had uncommitted changes
    - `command` - the command that was executed
    - `concurrency` - number of parallel workers used
    - `duration_secs` - total wall-clock duration in seconds
    - `exit_code` - exit code of the test command
  - `git_commit:<sha>` - reverse index mapping a commit SHA to a comma-separated list of run IDs

  All fields in the JSON object are optional. Repositories without this file are handled gracefully; metadata will simply not be available.

* `repo.conf`: This file contains user configuration settings for the repository.
  `inq repo-config` will dump a repo configration and `inq help repo-config` has online help for all the repository settings.
