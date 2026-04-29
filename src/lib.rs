#![deny(missing_docs)]

//! inquest - A repository of test results
//!
//! Inquest started as a Rust port of the Python testrepository tool, maintaining
//! complete on-disk format compatibility with the original.
//!
//! # Overview
//!
//! inquest provides a database of test results which can be used as part of
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
//! - [`config`]: Configuration file parsing (inquest.toml / .testr.conf)
//! - [`testcommand`]: Test execution framework
//! - [`ui`]: User interface abstraction for output
//! - [`error`]: Error types and Result alias
//!
//! # Repository Formats
//!
//! ## Inquest format (default)
//!
//! The `.inquest/` directory contains:
//!
//! - `format`: Version file containing "1"
//! - `metadata.db`: SQLite database with run metadata, test results, times, and failing tests
//! - `runs/0`, `runs/1`, ...: Individual test run files in subunit v2 binary format
//!
//! ## Legacy format
//!
//! The `.testrepository/` directory is also supported for backwards compatibility
//! with the Python version of testrepository.
//!
//! # Example
//!
//! ```no_run
//! use inquest::repository::{RepositoryFactory, inquest::InquestRepositoryFactory};
//! use inquest::commands::{Command, InitCommand, StatsCommand};
//! use inquest::ui::UI;
//! use std::path::Path;
//!
//! # fn main() -> inquest::error::Result<()> {
//! // Initialize a new repository
//! let factory = InquestRepositoryFactory;
//! let repo = factory.initialise(Path::new("."))?;
//!
//! // Commands can be executed via the Command trait
//! struct SimpleUI;
//! impl UI for SimpleUI {
//!     fn output(&mut self, msg: &str) -> inquest::error::Result<()> {
//!         println!("{}", msg);
//!         Ok(())
//!     }
//!     fn error(&mut self, msg: &str) -> inquest::error::Result<()> {
//!         eprintln!("Error: {}", msg);
//!         Ok(())
//!     }
//!     fn warning(&mut self, msg: &str) -> inquest::error::Result<()> {
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

pub mod abbreviation;
pub mod commands;
pub mod config;
pub mod error;
pub mod eta;
pub mod grouping;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod ordering;
pub mod partition;
pub mod repository;
pub mod subunit_stream;
pub mod test_executor;
pub mod test_runner;
pub mod testcommand;
pub mod testlist;
pub mod ui;
pub mod watchdog;
#[cfg(feature = "web")]
pub mod web;

pub use error::{Error, Result};
