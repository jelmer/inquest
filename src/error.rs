//! Error types for testrepository

use std::io;
use std::path::PathBuf;
use thiserror::Error;

/// Result type alias for testrepository operations
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type for testrepository
#[derive(Error, Debug)]
pub enum Error {
    /// Repository was not found at the specified path.
    #[error("Repository not found at {0}")]
    RepositoryNotFound(PathBuf),

    /// Repository already exists at the specified path.
    #[error("Repository already exists at {0}")]
    RepositoryExists(PathBuf),

    /// Repository has an invalid format or version.
    #[error("Invalid repository format: {0}")]
    InvalidFormat(String),

    /// The requested test run ID was not found.
    #[error("Test run not found: {0}")]
    TestRunNotFound(String),

    /// The repository contains no test runs.
    #[error("No test runs in repository")]
    NoTestRuns,

    /// Configuration file error or invalid configuration.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Test command execution failed.
    #[error("Command execution failed: {0}")]
    CommandExecution(String),

    /// Failed to parse test output or data.
    #[error("Parse error: {0}")]
    Parse(String),

    /// I/O operation failed.
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// Database operation failed.
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// Subunit protocol error or invalid subunit stream.
    #[error("Subunit protocol error: {0}")]
    Subunit(String),

    /// Other error with custom message.
    #[error("{0}")]
    Other(String),
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::Other(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error::Other(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = Error::RepositoryNotFound(PathBuf::from("/tmp/test"));
        assert_eq!(err.to_string(), "Repository not found at /tmp/test");
    }

    #[test]
    fn test_error_from_string() {
        let err: Error = "custom error".into();
        assert_eq!(err.to_string(), "custom error");
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
    }
}
