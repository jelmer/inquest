//! Mirror inquest test runs into a side ref in git.
//!
//! When the `mirror_to_git_notes` config flag is enabled, each completed run
//! is published into the ref [`DEFAULT_MIRROR_REF`] as three blobs — the raw
//! subunit byte stream, a JSON metadata document, and (when non-empty) the
//! captured stderr — under the path
//!
//! ```text
//! <commit-sha>/<run-id>/subunit
//! <commit-sha>/<run-id>/metadata.json
//! <commit-sha>/<run-id>/stderr
//! ```
//!
//! The mirror is best-effort and one-way: `.inquest/` remains the source of
//! truth, and any failure inside this module is reported as `Err(...)` so the
//! caller can log-and-swallow. A run never fails because of a mirroring
//! problem.
//!
//! See `devnotes/git-mirror-design.md` for the full design.
//!
//! The module name is historical; this is no longer based on git notes.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::repository::{Repository, RunId, RunMetadata, TestRun};

/// The git ref under which inquest publishes mirrored runs.
pub const DEFAULT_MIRROR_REF: &str = "refs/inquest";

/// Schema version embedded in each `metadata.json` blob.
const METADATA_FORMAT_VERSION: u32 = 1;

/// Summary counts written into the metadata blob alongside [`RunMetadata`].
///
/// Computed from a [`TestRun`] before the mirror is invoked so that a
/// consumer can answer "did this run pass?" without parsing the subunit
/// stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunTotals {
    /// Total number of tests recorded in the run.
    pub total: usize,
    /// Tests whose status is [`crate::repository::TestStatus::Failure`].
    pub failures: usize,
    /// Tests whose status is [`crate::repository::TestStatus::Error`].
    pub errors: usize,
    /// Tests whose status is [`crate::repository::TestStatus::Skip`].
    pub skips: usize,
}

impl RunTotals {
    /// Compute totals from a [`TestRun`].
    pub fn from_run(run: &TestRun) -> Self {
        use crate::repository::TestStatus;
        let mut totals = RunTotals {
            total: run.total_tests(),
            ..RunTotals::default()
        };
        for result in run.results.values() {
            match result.status {
                TestStatus::Failure => totals.failures += 1,
                TestStatus::Error => totals.errors += 1,
                TestStatus::Skip => totals.skips += 1,
                _ => {}
            }
        }
        totals
    }
}

/// JSON shape of the `metadata.json` blob written for each mirrored run.
///
/// Field names are stable across schema versions; new fields are added
/// behind `#[serde(skip_serializing_if = "Option::is_none")]` so older
/// readers keep working.
#[derive(Debug, Serialize)]
struct MetadataDoc<'a> {
    format: u32,
    run_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    concurrency: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_args: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_dirty: Option<bool>,
    totals: TotalsDoc,
}

#[derive(Debug, Serialize)]
struct TotalsDoc {
    total: usize,
    failures: usize,
    errors: usize,
    skips: usize,
}

impl From<RunTotals> for TotalsDoc {
    fn from(t: RunTotals) -> Self {
        TotalsDoc {
            total: t.total,
            failures: t.failures,
            errors: t.errors,
            skips: t.skips,
        }
    }
}

/// Mirror a single completed run into the side ref `mirror_ref`.
///
/// Reads the run's subunit stream from `repo`, builds the metadata JSON,
/// and splices `<commit>/<run_id>/{subunit,metadata.json,stderr}` into the
/// existing root tree (creating the ref if it doesn't exist yet).
///
/// `stderr_bytes` is `None` when no stderr was captured and `Some(&[])` when
/// the captured stderr was empty; both cases omit the `stderr` blob.
///
/// The function is intentionally permissive about its environment:
///
/// - Returns `Ok(())` (a no-op) when the working directory is not inside a
///   git repository, or when `metadata.git_commit` is `None`. Both are
///   normal conditions, not errors.
/// - Any underlying git or I/O failure is reported as `Err(...)` so the
///   caller can decide whether to log-and-swallow.
///
/// Callers in production paths should treat any `Err` as a `tracing::warn!`
/// and continue — mirroring failure must not fail the run.
pub fn mirror_run(
    repo: &dyn Repository,
    run_id: &RunId,
    metadata: &RunMetadata,
    totals: RunTotals,
    timestamp: Option<&str>,
    stderr_bytes: Option<&[u8]>,
    mirror_ref: &str,
) -> Result<()> {
    let Some(commit) = metadata.git_commit.as_deref() else {
        tracing::debug!(
            "skipping git mirror for run {}: no git_commit recorded",
            run_id
        );
        return Ok(());
    };

    if !is_git_repo()? {
        tracing::info!("skipping git mirror: not inside a git repository");
        return Ok(());
    }

    // Hash the data blobs.
    let mut subunit_bytes = Vec::new();
    let mut reader = repo.get_test_run_raw(run_id)?;
    std::io::copy(&mut reader, &mut subunit_bytes)?;
    let subunit_oid = write_blob(&subunit_bytes)?;

    let doc = MetadataDoc {
        format: METADATA_FORMAT_VERSION,
        run_id: run_id.as_str(),
        timestamp,
        command: metadata.command.as_deref(),
        concurrency: metadata.concurrency,
        duration_secs: metadata.duration_secs,
        exit_code: metadata.exit_code,
        test_args: metadata.test_args.as_deref(),
        profile: metadata.profile.as_deref(),
        git_dirty: metadata.git_dirty,
        totals: totals.into(),
    };
    let metadata_json = serde_json::to_vec_pretty(&doc)
        .map_err(|e| Error::Other(format!("failed to serialize mirror metadata: {}", e)))?;
    let metadata_oid = write_blob(&metadata_json)?;

    let stderr_oid = match stderr_bytes {
        Some(bytes) if !bytes.is_empty() => Some(write_blob(bytes)?),
        _ => None,
    };

    // Splice into the existing root tree (or start from scratch).
    let parent_commit = read_ref(mirror_ref)?;
    let new_root = splice_run_into_root(
        parent_commit.as_deref(),
        commit,
        run_id.as_str(),
        &subunit_oid,
        &metadata_oid,
        stderr_oid.as_deref(),
    )?;

    let new_commit = commit_tree(
        &new_root,
        parent_commit.as_deref(),
        &format!("inquest run {} on {}", run_id.as_str(), commit),
    )?;
    update_ref(mirror_ref, &new_commit, parent_commit.as_deref())?;
    Ok(())
}

/// Build a new root tree by loading the existing one (if any), wiping the
/// `<commit>/<run_id>/` subtree (so vestigial files from a prior mirror of
/// this exact pair don't survive), and adding the fresh blobs.
fn splice_run_into_root(
    parent_commit: Option<&str>,
    commit: &str,
    run_id: &str,
    subunit_oid: &str,
    metadata_oid: &str,
    stderr_oid: Option<&str>,
) -> Result<String> {
    let index_dir = tempfile::TempDir::new()
        .map_err(|e| Error::Other(format!("creating temporary index dir: {}", e)))?;
    let index_path = index_dir.path().join("index");

    if let Some(parent) = parent_commit {
        // Load the parent commit's tree into the temp index.
        run_git_with_index(
            &index_path,
            &["read-tree", &format!("{}^{{tree}}", parent)],
            None,
        )?;
        // Drop any prior mirror of this exact (commit, run_id). `--cached`
        // operates only on the index, `--ignore-unmatch` makes the
        // first-mirror case a no-op rather than an error.
        run_git_with_index(
            &index_path,
            &[
                "rm",
                "--cached",
                "-r",
                "--quiet",
                "--ignore-unmatch",
                "--",
                &format!("{}/{}", commit, run_id),
            ],
            None,
        )?;
    }

    // Add the new entries.
    let entries = std::iter::once(("subunit", subunit_oid))
        .chain(std::iter::once(("metadata.json", metadata_oid)))
        .chain(stderr_oid.map(|oid| ("stderr", oid)));
    for (name, oid) in entries {
        let path = format!("{}/{}/{}", commit, run_id, name);
        let cacheinfo = format!("100644,{},{}", oid, path);
        run_git_with_index(
            &index_path,
            &["update-index", "--add", "--cacheinfo", &cacheinfo],
            None,
        )?;
    }

    let tree_oid_bytes = run_git_with_index(&index_path, &["write-tree"], None)?;
    drop(index_dir);
    Ok(String::from_utf8_lossy(&tree_oid_bytes).trim().to_string())
}

/// Run `git <args>` against the temp index at `index_path`, returning
/// captured stdout. Stdin is fed `stdin_bytes` when supplied.
fn run_git_with_index(
    index_path: &Path,
    args: &[&str],
    stdin_bytes: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.args(args).env("GIT_INDEX_FILE", index_path);
    if stdin_bytes.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Other(format!("failed to spawn `git {}`: {}", args[0], e)))?;
    if let Some(bytes) = stdin_bytes {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| Error::Other(format!("git {} stdin unavailable", args[0])))?;
        stdin.write_all(bytes)?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| Error::Other(format!("waiting on `git {}` failed: {}", args[0], e)))?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// Probe whether the current working directory is inside a git working tree
/// or a bare repository. Returns `Ok(false)` (not `Err`) when `git` exits
/// non-zero or is missing — those are user-visible conditions that the
/// caller turns into a no-op, not a hard failure.
fn is_git_repo() -> Result<bool> {
    match Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => Ok(status.success()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Write `bytes` to the git object store as a blob and return its OID.
fn write_blob(bytes: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Other(format!("failed to spawn `git hash-object`: {}", e)))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| Error::Other("git hash-object stdin unavailable".into()))?;
        stdin.write_all(bytes)?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| Error::Other(format!("waiting on `git hash-object` failed: {}", e)))?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if oid.is_empty() {
        return Err(Error::Other(
            "git hash-object returned empty OID".to_string(),
        ));
    }
    Ok(oid)
}

/// Resolve `ref_name` to a commit OID. Returns `None` when the ref does
/// not exist yet.
fn read_ref(ref_name: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", ref_name])
        .output()
        .map_err(|e| Error::Other(format!("failed to spawn `git rev-parse`: {}", e)))?;
    if !output.status.success() {
        return Ok(None);
    }
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if oid.is_empty() {
        Ok(None)
    } else {
        Ok(Some(oid))
    }
}

/// Create a commit whose tree is `tree_oid`, with optional `parent`.
/// Author/committer are set to a fixed inquest identity via env vars so
/// we don't need to depend on the user's `user.email` / `user.name`. We
/// also force `commit.gpgsign=false` for the same reason — mirror commits
/// are bookkeeping, not user-authored history.
fn commit_tree(tree_oid: &str, parent: Option<&str>, message: &str) -> Result<String> {
    let mut args: Vec<String> = vec!["commit-tree".into(), tree_oid.into()];
    if let Some(p) = parent {
        args.push("-p".into());
        args.push(p.into());
    }
    args.push("-m".into());
    args.push(message.into());

    let output = Command::new("git")
        .args(&args)
        .env("GIT_AUTHOR_NAME", "inquest")
        .env("GIT_AUTHOR_EMAIL", "inquest@localhost")
        .env("GIT_COMMITTER_NAME", "inquest")
        .env("GIT_COMMITTER_EMAIL", "inquest@localhost")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "false")
        .output()
        .map_err(|e| Error::Other(format!("failed to spawn `git commit-tree`: {}", e)))?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Atomically move `ref_name` from `expected` to `new_oid`. When `expected`
/// is `None`, the ref is created (the all-zeros OID is the standard
/// "ref must not exist" sentinel for `update-ref`).
fn update_ref(ref_name: &str, new_oid: &str, expected: Option<&str>) -> Result<()> {
    let zero = "0000000000000000000000000000000000000000";
    let expected_arg = expected.unwrap_or(zero);
    let output = Command::new("git")
        .args(["update-ref", ref_name, new_oid, expected_arg])
        .output()
        .map_err(|e| Error::Other(format!("failed to spawn `git update-ref`: {}", e)))?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "git update-ref failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, TestId, TestResult, TestStatus};
    use std::path::Path;
    use tempfile::TempDir;

    /// Run `git` with `args` inside `cwd`, asserting it succeeds. Returns
    /// stdout as a `Vec<u8>` so tests can compare binary blobs byte-for-byte.
    fn git_in(cwd: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn git {:?}: {}", args, e));
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    /// Run `git` and return stdout as a trimmed `String`.
    fn git_str(cwd: &Path, args: &[&str]) -> String {
        let bytes = git_in(cwd, args);
        String::from_utf8(bytes).unwrap().trim().to_string()
    }

    /// `git init` in `cwd` and make one empty commit so HEAD points to
    /// something the mirror can attach data to.
    fn init_git_repo(cwd: &Path) -> String {
        git_in(cwd, &["init", "-q", "-b", "main"]);
        // Local identity & no signing: tests must not depend on a user's
        // global git config (which on dev machines often signs commits).
        git_in(cwd, &["config", "user.email", "tests@example.com"]);
        git_in(cwd, &["config", "user.name", "tests"]);
        git_in(cwd, &["config", "commit.gpgsign", "false"]);
        git_in(cwd, &["config", "tag.gpgsign", "false"]);
        git_in(cwd, &["commit", "--allow-empty", "-m", "init", "-q"]);
        git_str(cwd, &["rev-parse", "HEAD"])
    }

    /// Build an inquest repository in `cwd/.inquest` with one run whose
    /// subunit stream contains `payload`.
    fn make_repo_with_run(cwd: &Path, payload: &[u8]) -> (Box<dyn Repository>, RunId) {
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(cwd).unwrap();
        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        writer.write_all(payload).unwrap();
        writer.flush().unwrap();
        drop(writer);
        (repo, run_id)
    }

    /// Construct a `RunMetadata` with the given commit hash; everything else
    /// uses sensible defaults so tests don't need to write boilerplate.
    fn metadata_for(commit: &str) -> RunMetadata {
        RunMetadata {
            git_commit: Some(commit.to_string()),
            git_dirty: Some(false),
            command: Some("test".to_string()),
            concurrency: Some(1),
            duration_secs: Some(0.5),
            exit_code: Some(0),
            test_args: None,
            profile: None,
            predicted_duration_secs: None,
        }
    }

    /// Look up the OID of a path inside the mirror ref's root tree.
    /// Returns `None` when the entry is missing.
    fn lookup_entry(cwd: &Path, mirror_ref: &str, path: &str) -> Option<String> {
        let spec = format!("{}^{{tree}}", mirror_ref);
        let output = Command::new("git")
            .args(["ls-tree", "-r", &spec])
            .current_dir(cwd)
            .output()
            .unwrap();
        if !output.status.success() {
            return None;
        }
        let listing = String::from_utf8_lossy(&output.stdout).into_owned();
        for line in listing.lines() {
            // "<mode> <kind> <oid>\t<path>"
            let (header, entry_path) = line.split_once('\t')?;
            if entry_path == path {
                let oid = header.split_whitespace().nth(2)?;
                return Some(oid.to_string());
            }
        }
        None
    }

    /// Run the mirror with `cwd` set to the test repo so the shelled-out
    /// `git` calls land in the right place.
    fn mirror_with_cwd(
        cwd: &Path,
        repo: &dyn Repository,
        run_id: &RunId,
        metadata: &RunMetadata,
        totals: RunTotals,
        stderr_bytes: Option<&[u8]>,
    ) -> Result<()> {
        // The functions in this module shell out via `Command::new("git")`,
        // which uses the *process's* working directory. Tests run in
        // parallel, so we briefly take a process-wide lock around the
        // `set_current_dir` + mirror call.
        let _guard = TEST_CWD_LOCK.lock().unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(cwd).unwrap();
        let result = mirror_run(
            repo,
            run_id,
            metadata,
            totals,
            None,
            stderr_bytes,
            DEFAULT_MIRROR_REF,
        );
        std::env::set_current_dir(original).unwrap();
        result
    }

    static TEST_CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn mirror_round_trip_on_fresh_repo() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();
        let commit = init_git_repo(cwd);

        let payload = b"\xb3subunit-bytes-here";
        let (repo, run_id) = make_repo_with_run(cwd, payload);
        let metadata = metadata_for(&commit);

        mirror_with_cwd(cwd, &*repo, &run_id, &metadata, RunTotals::default(), None).unwrap();

        let subunit_path = format!("{}/{}/subunit", commit, run_id.as_str());
        let metadata_path = format!("{}/{}/metadata.json", commit, run_id.as_str());
        let stderr_path = format!("{}/{}/stderr", commit, run_id.as_str());

        let subunit_oid =
            lookup_entry(cwd, DEFAULT_MIRROR_REF, &subunit_path).expect("subunit entry");
        let metadata_oid =
            lookup_entry(cwd, DEFAULT_MIRROR_REF, &metadata_path).expect("metadata entry");

        // Subunit blob round-trips byte-for-byte.
        let blob = git_in(cwd, &["cat-file", "-p", &subunit_oid]);
        assert_eq!(blob, payload);

        // Metadata blob is well-formed JSON with the expected fields.
        let meta_bytes = git_in(cwd, &["cat-file", "-p", &metadata_oid]);
        let value: serde_json::Value = serde_json::from_slice(&meta_bytes).unwrap();
        assert_eq!(value["format"], 1);
        assert_eq!(value["run_id"], run_id.as_str());
        assert_eq!(value["command"], "test");

        // No stderr was supplied, so the entry must not exist.
        assert_eq!(lookup_entry(cwd, DEFAULT_MIRROR_REF, &stderr_path), None);
    }

    #[test]
    fn mirror_two_runs_share_the_root_tree() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();
        let commit = init_git_repo(cwd);

        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(cwd).unwrap();

        let (run0, mut w) = repo.begin_test_run_raw().unwrap();
        w.write_all(b"first").unwrap();
        drop(w);
        let metadata = metadata_for(&commit);
        mirror_with_cwd(cwd, &*repo, &run0, &metadata, RunTotals::default(), None).unwrap();

        let (run1, mut w) = repo.begin_test_run_raw().unwrap();
        w.write_all(b"second").unwrap();
        drop(w);
        mirror_with_cwd(cwd, &*repo, &run1, &metadata, RunTotals::default(), None).unwrap();

        let oid0 = lookup_entry(
            cwd,
            DEFAULT_MIRROR_REF,
            &format!("{}/{}/subunit", commit, run0.as_str()),
        )
        .expect("first run preserved");
        let oid1 = lookup_entry(
            cwd,
            DEFAULT_MIRROR_REF,
            &format!("{}/{}/subunit", commit, run1.as_str()),
        )
        .expect("second run added");
        assert_ne!(oid0, oid1);
        assert_eq!(git_in(cwd, &["cat-file", "-p", &oid0]), b"first");
        assert_eq!(git_in(cwd, &["cat-file", "-p", &oid1]), b"second");
    }

    #[test]
    fn re_mirror_replaces_only_the_target_run() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();
        let commit = init_git_repo(cwd);

        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(cwd).unwrap();

        let (run0, mut w) = repo.begin_test_run_raw().unwrap();
        w.write_all(b"first").unwrap();
        drop(w);
        let metadata = metadata_for(&commit);
        mirror_with_cwd(cwd, &*repo, &run0, &metadata, RunTotals::default(), None).unwrap();

        // Manually overwrite the on-disk subunit file so the next mirror
        // sees different bytes for the same run id.
        let run0_path = cwd.join(".inquest/runs").join(run0.as_str());
        std::fs::write(&run0_path, b"replacement").unwrap();

        // Re-mirror run0 — its subunit blob should be rewritten in place.
        mirror_with_cwd(cwd, &*repo, &run0, &metadata, RunTotals::default(), None).unwrap();
        let oid0 = lookup_entry(
            cwd,
            DEFAULT_MIRROR_REF,
            &format!("{}/{}/subunit", commit, run0.as_str()),
        )
        .unwrap();
        assert_eq!(git_in(cwd, &["cat-file", "-p", &oid0]), b"replacement");
    }

    #[test]
    fn re_mirror_drops_vestigial_stderr() {
        // First mirror with stderr; second mirror with no stderr should
        // remove the previous stderr blob from the tree.
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();
        let commit = init_git_repo(cwd);

        let (repo, run_id) = make_repo_with_run(cwd, b"bytes");
        let metadata = metadata_for(&commit);

        mirror_with_cwd(
            cwd,
            &*repo,
            &run_id,
            &metadata,
            RunTotals::default(),
            Some(b"runner crashed here\n"),
        )
        .unwrap();
        let stderr_path = format!("{}/{}/stderr", commit, run_id.as_str());
        assert!(
            lookup_entry(cwd, DEFAULT_MIRROR_REF, &stderr_path).is_some(),
            "stderr blob should exist after first mirror"
        );

        mirror_with_cwd(cwd, &*repo, &run_id, &metadata, RunTotals::default(), None).unwrap();
        assert_eq!(
            lookup_entry(cwd, DEFAULT_MIRROR_REF, &stderr_path),
            None,
            "stderr blob should be gone after re-mirror with no stderr"
        );
    }

    #[test]
    fn empty_stderr_is_omitted() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();
        let commit = init_git_repo(cwd);

        let (repo, run_id) = make_repo_with_run(cwd, b"bytes");
        let metadata = metadata_for(&commit);

        mirror_with_cwd(
            cwd,
            &*repo,
            &run_id,
            &metadata,
            RunTotals::default(),
            Some(b""),
        )
        .unwrap();
        let stderr_path = format!("{}/{}/stderr", commit, run_id.as_str());
        assert_eq!(lookup_entry(cwd, DEFAULT_MIRROR_REF, &stderr_path), None);
    }

    #[test]
    fn stderr_round_trips() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();
        let commit = init_git_repo(cwd);

        let (repo, run_id) = make_repo_with_run(cwd, b"bytes");
        let metadata = metadata_for(&commit);
        let stderr = b"line one\nline two\n";

        mirror_with_cwd(
            cwd,
            &*repo,
            &run_id,
            &metadata,
            RunTotals::default(),
            Some(stderr),
        )
        .unwrap();

        let oid = lookup_entry(
            cwd,
            DEFAULT_MIRROR_REF,
            &format!("{}/{}/stderr", commit, run_id.as_str()),
        )
        .expect("stderr entry");
        assert_eq!(git_in(cwd, &["cat-file", "-p", &oid]), stderr);
    }

    #[test]
    fn mirror_is_a_noop_outside_a_git_repo() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();
        // Deliberately do *not* `git init`.
        let payload = b"bytes";
        let (repo, run_id) = make_repo_with_run(cwd, payload);
        let metadata = metadata_for("0000000000000000000000000000000000000000");
        // No error, no panic, no ref created.
        mirror_with_cwd(cwd, &*repo, &run_id, &metadata, RunTotals::default(), None).unwrap();
    }

    #[test]
    fn mirror_is_a_noop_when_git_commit_is_missing() {
        let temp = TempDir::new().unwrap();
        let cwd = temp.path();
        let _commit = init_git_repo(cwd);
        let (repo, run_id) = make_repo_with_run(cwd, b"bytes");
        let metadata = RunMetadata::default(); // git_commit: None
        mirror_with_cwd(cwd, &*repo, &run_id, &metadata, RunTotals::default(), None).unwrap();

        // No mirror ref should have been created.
        let output = Command::new("git")
            .args(["rev-parse", "--verify", DEFAULT_MIRROR_REF])
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "mirror ref unexpectedly exists: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn run_totals_from_run_counts_correctly() {
        let mut run = TestRun::new(RunId::new("0"));
        run.add_result(TestResult {
            test_id: TestId::new("a"),
            status: TestStatus::Success,
            duration: None,
            message: None,
            details: None,
            tags: vec![],
        });
        run.add_result(TestResult {
            test_id: TestId::new("b"),
            status: TestStatus::Failure,
            duration: None,
            message: None,
            details: None,
            tags: vec![],
        });
        run.add_result(TestResult {
            test_id: TestId::new("c"),
            status: TestStatus::Error,
            duration: None,
            message: None,
            details: None,
            tags: vec![],
        });
        run.add_result(TestResult {
            test_id: TestId::new("d"),
            status: TestStatus::Skip,
            duration: None,
            message: None,
            details: None,
            tags: vec![],
        });
        let totals = RunTotals::from_run(&run);
        assert_eq!(totals.total, 4);
        assert_eq!(totals.failures, 1);
        assert_eq!(totals.errors, 1);
        assert_eq!(totals.skips, 1);
    }
}
