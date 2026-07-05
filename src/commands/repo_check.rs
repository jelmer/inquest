//! Check that a repository is present and openable.
//!
//! Intended for CI use: exits 0 when there is nothing wrong (repository
//! opens cleanly, or is simply absent), and non-zero when a `.inquest/`
//! directory exists but cannot be opened. Callers that restore a
//! `.inquest/` from a cache can use this to detect a corrupt restore
//! before it makes the next `inq` invocation fail with a confusing
//! error, and then decide for themselves whether to discard it.

use crate::commands::Command;
use crate::error::{Error, Result};
use crate::repository::inquest::InquestRepositoryFactory;
use crate::repository::RepositoryFactory;
use crate::ui::UI;
use std::path::Path;

/// Check that a repository at the given path can be opened.
pub struct RepoCheckCommand {
    base_path: Option<String>,
}

impl RepoCheckCommand {
    /// Create a new repo-check command targeting `base_path`
    /// (current directory when `None`).
    pub fn new(base_path: Option<String>) -> Self {
        RepoCheckCommand { base_path }
    }
}

impl Command for RepoCheckCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = self
            .base_path
            .as_deref()
            .map_or_else(|| Path::new("."), Path::new);
        // Check the inquest-native layout only. This command is used by
        // CI wrappers to sanity-check a restored `.inquest/`, and the
        // legacy `.testrepository/` fallback would just be noise here.
        match InquestRepositoryFactory.open(base) {
            Ok(_) => {
                ui.output("Repository OK")?;
                Ok(0)
            }
            Err(Error::RepositoryNotFound(path)) => {
                ui.output(&format!("No repository at {}", path.display()))?;
                Ok(0)
            }
            Err(e) => {
                ui.error(&format!(
                    "Repository at {} is not usable: {}",
                    base.join(".inquest").display(),
                    e
                ))?;
                Ok(1)
            }
        }
    }

    fn name(&self) -> &str {
        "repo-check"
    }

    fn help(&self) -> &str {
        "Check that the test repository is present and openable"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::RepositoryFactory;
    use crate::ui::test_ui::TestUI;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn ok_when_no_repository() {
        let temp = TempDir::new().unwrap();
        let mut ui = TestUI::new();
        let cmd = RepoCheckCommand::new(Some(temp.path().to_string_lossy().to_string()));
        assert_eq!(cmd.execute(&mut ui).unwrap(), 0);
    }

    #[test]
    fn ok_when_repository_is_healthy() {
        let temp = TempDir::new().unwrap();
        InquestRepositoryFactory.initialise(temp.path()).unwrap();
        let mut ui = TestUI::new();
        let cmd = RepoCheckCommand::new(Some(temp.path().to_string_lossy().to_string()));
        assert_eq!(cmd.execute(&mut ui).unwrap(), 0);
    }

    #[test]
    fn fails_when_metadata_missing() {
        let temp = TempDir::new().unwrap();
        InquestRepositoryFactory.initialise(temp.path()).unwrap();
        fs::remove_file(temp.path().join(".inquest/metadata.db")).unwrap();
        let mut ui = TestUI::new();
        let cmd = RepoCheckCommand::new(Some(temp.path().to_string_lossy().to_string()));
        assert_eq!(cmd.execute(&mut ui).unwrap(), 1);
    }

    #[test]
    fn fails_when_format_missing() {
        let temp = TempDir::new().unwrap();
        InquestRepositoryFactory.initialise(temp.path()).unwrap();
        fs::remove_file(temp.path().join(".inquest/format")).unwrap();
        let mut ui = TestUI::new();
        let cmd = RepoCheckCommand::new(Some(temp.path().to_string_lossy().to_string()));
        assert_eq!(cmd.execute(&mut ui).unwrap(), 1);
    }
}
