//! testrepository - A repository of test results
//!
//! This is a Rust port of the Python testrepository tool, maintaining
//! complete on-disk format compatibility with the original.
//!
//! # Overview
//!
//! testrepository provides a database of test results which can be used as part of
//! developer workflow to track test history, identify failing tests, and analyze
//! test performance over time.
//!
//! # Architecture
//!
//! The library is organized into several key modules:
//!
//! - [`repository`]: Core repository trait and file-based implementation for storing test results
//! - [`commands`]: All user-facing commands (init, run, load, last, failing, stats, slowest, list-tests)
//! - [`subunit_stream`]: Subunit v2 protocol parsing and generation
//! - [`config`]: .testr.conf configuration file parsing
//! - [`testcommand`]: Test execution framework
//! - [`ui`]: User interface abstraction for output
//! - [`error`]: Error types and Result alias
//!
//! # Repository Format
//!
//! The `.testrepository/` directory contains:
//!
//! - `format`: Version file containing "1"
//! - `next-stream`: Counter for the next run ID
//! - `0`, `1`, `2`, ...: Individual test run files in subunit v2 binary format
//!
//! This format is fully compatible with the Python version of testrepository.
//!
//! # Example
//!
//! ```no_run
//! use testrepository::repository::{RepositoryFactory, file::FileRepositoryFactory};
//! use testrepository::commands::{Command, InitCommand, StatsCommand};
//! use testrepository::ui::UI;
//! use std::path::Path;
//!
//! # fn main() -> testrepository::error::Result<()> {
//! // Initialize a new repository
//! let factory = FileRepositoryFactory;
//! let repo = factory.initialise(Path::new("."))?;
//!
//! // Commands can be executed via the Command trait
//! struct SimpleUI;
//! impl UI for SimpleUI {
//!     fn output(&mut self, msg: &str) -> testrepository::error::Result<()> {
//!         println!("{}", msg);
//!         Ok(())
//!     }
//!     fn error(&mut self, msg: &str) -> testrepository::error::Result<()> {
//!         eprintln!("Error: {}", msg);
//!         Ok(())
//!     }
//!     fn warning(&mut self, msg: &str) -> testrepository::error::Result<()> {
//!         eprintln!("Warning: {}", msg);
//!         Ok(())
//!     }
//! }
//!
//! let mut ui = SimpleUI;
//! let stats_cmd = StatsCommand::new(None);
//! stats_cmd.execute(&mut ui)?;
//! # Ok(())
//! # }
//! ```

pub mod commands;
pub mod config;
pub mod error;
pub mod partition;
pub mod repository;
pub mod subunit_stream;
pub mod testcommand;
pub mod testlist;
pub mod ui;

pub use error::{Error, Result};
