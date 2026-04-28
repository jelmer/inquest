//! Bisect across recorded run history to find the commit that broke a test.
//!
//! Walks the repository's run history to identify the last recorded run in
//! which the target test passed and a commit where it failed, then drives
//! `git bisect run` over that range using `inq run <test>` as the script.

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::error::{Error, Result};
use crate::repository::{Repository, RunId, TestId};
use crate::ui::UI;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};

/// Outcome of scanning history for the target test.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HistoryScan {
    /// Most recent recorded run (with a git commit) where the target test passed.
    last_good: Option<(RunId, String)>,
    /// Most recent recorded run (with a git commit) where the target test failed.
    last_bad: Option<(RunId, String)>,
}

/// Drive `git bisect` for a failing test using recorded run history to seed
/// the good/bad endpoints.
pub struct BisectCommand {
    base_path: Option<String>,
    target_test: String,
    /// Optional explicit good commit override (skips history scan).
    good_commit: Option<String>,
    /// Optional explicit bad commit override (defaults to HEAD).
    bad_commit: Option<String>,
}

impl BisectCommand {
    /// Build a bisect command for the given test.
    pub fn new(base_path: Option<String>, target_test: String) -> Self {
        BisectCommand {
            base_path,
            target_test,
            good_commit: None,
            bad_commit: None,
        }
    }

    /// Override the good (passing) commit. When set, the run history is not
    /// scanned to find a passing baseline.
    pub fn with_good_commit(mut self, commit: Option<String>) -> Self {
        self.good_commit = commit;
        self
    }

    /// Override the bad (failing) commit. Defaults to `HEAD` when not set.
    pub fn with_bad_commit(mut self, commit: Option<String>) -> Self {
        self.bad_commit = commit;
        self
    }

    /// Walk recorded runs (newest first) classifying the target test.
    ///
    /// Stops scanning once both a good and a bad commit have been found —
    /// there is no point reading further history. Runs that don't carry a
    /// git commit, ran on a dirty tree, or didn't include the target test
    /// at all are skipped.
    fn scan_history(&self, repo: &dyn Repository) -> Result<HistoryScan> {
        let target = TestId::new(&self.target_test);
        let run_ids = repo.list_run_ids()?;

        let mut scan = HistoryScan {
            last_good: None,
            last_bad: None,
        };

        for run_id in run_ids.into_iter().rev() {
            if scan.last_good.is_some() && scan.last_bad.is_some() {
                break;
            }

            let metadata = repo.get_run_metadata(&run_id)?;
            let Some(commit) = metadata.git_commit else {
                continue;
            };
            // A dirty working tree means the recorded commit doesn't fully
            // describe what was tested, so it can't anchor a bisect range.
            if metadata.git_dirty.unwrap_or(false) {
                continue;
            }

            let run = match repo.get_test_run(&run_id) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("skipping run {} during bisect scan: {}", run_id, e);
                    continue;
                }
            };
            let Some(result) = run.results.get(&target) else {
                continue;
            };

            if result.status.is_failure() {
                if scan.last_bad.is_none() {
                    scan.last_bad = Some((run_id, commit));
                }
            } else if result.status.is_success() && scan.last_good.is_none() {
                scan.last_good = Some((run_id, commit));
            }
        }

        Ok(scan)
    }

    /// Resolve a git revspec to a full commit hash so the bisect range is
    /// stable even if the user provides a branch name or short hash.
    fn resolve_commit(base: &Path, revspec: &str) -> Result<String> {
        let output = ProcessCommand::new("git")
            .args(["rev-parse", "--verify", revspec])
            .current_dir(base)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| Error::CommandExecution(format!("Failed to run git rev-parse: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::CommandExecution(format!(
                "git rev-parse {} failed: {}",
                revspec,
                stderr.trim()
            )));
        }

        let stdout = String::from_utf8(output.stdout).map_err(|e| {
            Error::CommandExecution(format!("git rev-parse output not UTF-8: {}", e))
        })?;
        Ok(stdout.trim().to_string())
    }

    /// Run a `git bisect` subcommand from the given working directory.
    fn run_git_bisect(base: &Path, args: &[&str]) -> Result<std::process::Output> {
        let mut cmd_args = vec!["bisect"];
        cmd_args.extend_from_slice(args);
        ProcessCommand::new("git")
            .args(&cmd_args)
            .current_dir(base)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| Error::CommandExecution(format!("Failed to run git bisect: {}", e)))
    }

    /// Build the shell script that `git bisect run` invokes at each commit.
    ///
    /// The script invokes the running `inq` binary against the same
    /// repository directory and exits with `git bisect run`'s expected
    /// codes: 0 = good, 1 = bad, 125 = skip (cannot test this commit).
    fn build_bisect_script(&self, inq_path: &Path) -> Result<(tempfile::NamedTempFile, String)> {
        use std::io::Write;

        let mut file = tempfile::Builder::new()
            .prefix("inq-bisect-")
            .suffix(".sh")
            .tempfile()?;

        let dir_arg = match self.base_path.as_deref() {
            Some(p) => format!("-C {}", sh_quote(p)),
            None => String::new(),
        };

        // Use a regex-anchored filter so we only run the target test and
        // nothing whose ID happens to contain it as a substring.
        let pattern = format!("^{}$", regex::escape(&self.target_test));

        let script = format!(
            "#!/bin/sh\n\
             # Generated by `inq bisect`. Runs the target test against the current\n\
             # commit and translates inq's exit code to git bisect's vocabulary.\n\
             set -u\n\
             {inq} {dir_arg} run {filter} >/dev/null 2>&1\n\
             status=$?\n\
             if [ \"$status\" = 0 ]; then\n\
                 exit 0\n\
             elif [ \"$status\" = 1 ]; then\n\
                 exit 1\n\
             else\n\
                 # inq couldn't run the test at this commit (build failure,\n\
                 # discovery error, etc.) — tell git to skip this revision.\n\
                 exit 125\n\
             fi\n",
            inq = sh_quote(&inq_path.to_string_lossy()),
            dir_arg = dir_arg,
            filter = sh_quote(&pattern),
        );

        file.write_all(script.as_bytes())?;
        file.flush()?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = file.as_file().metadata()?.permissions();
            perms.set_mode(0o755);
            file.as_file().set_permissions(perms)?;
        }

        let path = file.path().to_string_lossy().to_string();
        Ok((file, path))
    }
}

impl Command for BisectCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = Path::new(self.base_path.as_deref().unwrap_or("."));

        let repo = open_repository(self.base_path.as_deref())?;

        ui.output(&format!("Bisecting test: {}", self.target_test))?;

        // Determine the bad commit. If the caller didn't pin one, prefer the
        // most recent recorded failure, falling back to HEAD.
        let scan = self.scan_history(&*repo)?;
        let bad_commit_revspec = match self.bad_commit.clone() {
            Some(c) => c,
            None => match &scan.last_bad {
                Some((run_id, commit)) => {
                    ui.output(&format!(
                        "Using run {} ({}) as the bad commit (target test failed)",
                        run_id,
                        short_commit(commit)
                    ))?;
                    commit.clone()
                }
                None => "HEAD".to_string(),
            },
        };

        let good_commit_revspec = match self.good_commit.clone() {
            Some(c) => c,
            None => match &scan.last_good {
                Some((run_id, commit)) => {
                    ui.output(&format!(
                        "Using run {} ({}) as the good commit (target test passed)",
                        run_id,
                        short_commit(commit)
                    ))?;
                    commit.clone()
                }
                None => {
                    return Err(Error::Other(format!(
                        "No recorded run found where '{}' passed. Pass a known-good commit \
                         explicitly, or record a passing run on an older commit first.",
                        self.target_test
                    )));
                }
            },
        };

        let bad_commit = Self::resolve_commit(base, &bad_commit_revspec)?;
        let good_commit = Self::resolve_commit(base, &good_commit_revspec)?;

        if bad_commit == good_commit {
            return Err(Error::Other(format!(
                "Good and bad commits are identical ({}); nothing to bisect",
                short_commit(&bad_commit)
            )));
        }

        // Drop the repository handle before invoking git bisect — the bisect
        // script will run `inq` itself, which needs to open the repository.
        drop(repo);

        // We need to know which `inq` binary to invoke from the script.
        // `current_exe` returns the binary path of the running process, which
        // matches the one the user just invoked.
        let inq_path = std::env::current_exe()
            .map_err(|e| Error::CommandExecution(format!("Cannot locate inq binary: {}", e)))?;

        let (script_file, script_path) = self.build_bisect_script(&inq_path)?;

        ui.output(&format!(
            "Starting git bisect: bad={}, good={}",
            short_commit(&bad_commit),
            short_commit(&good_commit)
        ))?;

        let start = Self::run_git_bisect(base, &["start", &bad_commit, &good_commit])?;
        if !start.status.success() {
            return Err(Error::CommandExecution(format!(
                "git bisect start failed: {}",
                String::from_utf8_lossy(&start.stderr).trim()
            )));
        }

        // From here we MUST clean up bisect state on every exit path or the
        // user is left in a detached HEAD with a half-finished bisect.
        let run_result = Self::run_git_bisect(base, &["run", &script_path]);

        // Capture any output git bisect produced before resetting — `bisect
        // reset` discards it from the bisect log otherwise.
        match &run_result {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if !stdout.trim().is_empty() {
                    ui.output(stdout.trim_end())?;
                }
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stderr.trim().is_empty() {
                    ui.output(stderr.trim_end())?;
                }
            }
            Err(e) => {
                ui.error(&format!("git bisect run failed: {}", e))?;
            }
        }

        let reset = Self::run_git_bisect(base, &["reset"]);
        if let Err(e) = reset {
            ui.error(&format!("git bisect reset failed: {}", e))?;
        }

        // Keep the script file alive until after git bisect finishes.
        drop(script_file);

        match run_result {
            Ok(out) if out.status.success() => Ok(0),
            Ok(out) => Ok(out.status.code().unwrap_or(1)),
            Err(e) => Err(e),
        }
    }

    fn name(&self) -> &str {
        "bisect"
    }

    fn help(&self) -> &str {
        "Bisect git history to find the commit that broke a test"
    }
}

/// Format a commit hash for display, falling back to the full hash if it's
/// already short.
fn short_commit(commit: &str) -> String {
    if commit.len() <= 12 {
        commit.to_string()
    } else {
        commit[..12].to_string()
    }
}

/// Single-quote a string for safe inclusion in a `/bin/sh` command line.
///
/// We wrap in single quotes and replace embedded single quotes with the
/// `'\''` escape sequence — the canonical POSIX-shell-safe encoding.
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, RunMetadata, TestResult, TestRun};
    use tempfile::TempDir;

    fn insert_run(
        repo: &mut dyn Repository,
        run_idx: i64,
        results: Vec<TestResult>,
        commit: Option<&str>,
        dirty: bool,
    ) -> RunId {
        let mut run = TestRun::new(RunId::new(run_idx.to_string()));
        run.timestamp = chrono::DateTime::from_timestamp(1_000_000_000 + run_idx, 0).unwrap();
        for r in results {
            run.add_result(r);
        }
        let id = repo.insert_test_run(run).unwrap();
        repo.set_run_metadata(
            &id,
            RunMetadata {
                git_commit: commit.map(|s| s.to_string()),
                git_dirty: Some(dirty),
                ..RunMetadata::default()
            },
        )
        .unwrap();
        id
    }

    #[test]
    fn test_bisect_command_name_and_help() {
        let cmd = BisectCommand::new(None, "test_foo".to_string());
        assert_eq!(cmd.name(), "bisect");
        assert!(!cmd.help().is_empty());
    }

    #[test]
    fn test_scan_history_finds_last_good_and_bad() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // Run 0: target passes at commit aaa
        insert_run(
            &mut *repo,
            0,
            vec![TestResult::success("test_foo")],
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            false,
        );
        // Run 1: target passes at commit bbb (newer good)
        insert_run(
            &mut *repo,
            1,
            vec![TestResult::success("test_foo")],
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            false,
        );
        // Run 2: target fails at commit ccc
        insert_run(
            &mut *repo,
            2,
            vec![TestResult::failure("test_foo", "boom")],
            Some("cccccccccccccccccccccccccccccccccccccccc"),
            false,
        );
        // Run 3: target fails again at commit ddd (newer bad)
        insert_run(
            &mut *repo,
            3,
            vec![TestResult::failure("test_foo", "boom")],
            Some("dddddddddddddddddddddddddddddddddddddddd"),
            false,
        );

        let cmd = BisectCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            "test_foo".to_string(),
        );
        let scan = cmd.scan_history(&*repo).unwrap();

        assert_eq!(
            scan.last_good,
            Some((
                RunId::new("1"),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()
            ))
        );
        assert_eq!(
            scan.last_bad,
            Some((
                RunId::new("3"),
                "dddddddddddddddddddddddddddddddddddddddd".to_string()
            ))
        );
    }

    #[test]
    fn test_scan_history_skips_dirty_runs() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // Newest passing run is dirty; older passing run is clean.
        insert_run(
            &mut *repo,
            0,
            vec![TestResult::success("test_foo")],
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            false,
        );
        insert_run(
            &mut *repo,
            1,
            vec![TestResult::success("test_foo")],
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            true, // dirty — should be skipped
        );

        let cmd = BisectCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            "test_foo".to_string(),
        );
        let scan = cmd.scan_history(&*repo).unwrap();

        assert_eq!(
            scan.last_good,
            Some((
                RunId::new("0"),
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()
            ))
        );
    }

    #[test]
    fn test_scan_history_skips_runs_without_target() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // Run that doesn't include test_foo at all.
        insert_run(
            &mut *repo,
            0,
            vec![TestResult::success("test_other")],
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            false,
        );
        // Run that does.
        insert_run(
            &mut *repo,
            1,
            vec![TestResult::success("test_foo")],
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            false,
        );

        let cmd = BisectCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            "test_foo".to_string(),
        );
        let scan = cmd.scan_history(&*repo).unwrap();

        assert_eq!(
            scan.last_good,
            Some((
                RunId::new("1"),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()
            ))
        );
        assert!(scan.last_bad.is_none());
    }

    #[test]
    fn test_scan_history_skips_runs_without_commit() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        insert_run(
            &mut *repo,
            0,
            vec![TestResult::success("test_foo")],
            None,
            false,
        );

        let cmd = BisectCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            "test_foo".to_string(),
        );
        let scan = cmd.scan_history(&*repo).unwrap();
        assert!(scan.last_good.is_none());
        assert!(scan.last_bad.is_none());
    }

    #[test]
    fn test_short_commit() {
        assert_eq!(short_commit("abc"), "abc");
        assert_eq!(
            short_commit("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "aaaaaaaaaaaa"
        );
    }

    #[test]
    fn test_sh_quote_simple() {
        assert_eq!(sh_quote("foo"), "'foo'");
        assert_eq!(sh_quote("foo bar"), "'foo bar'");
    }

    #[test]
    fn test_sh_quote_embedded_single_quote() {
        // The 'don'\''t' encoding is the canonical POSIX form: close the
        // outer quote, escape one literal apostrophe, reopen.
        assert_eq!(sh_quote("don't"), "'don'\\''t'");
    }
}
