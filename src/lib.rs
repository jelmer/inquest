//! testrepository - A repository of test results
//!
//! This is a Rust port of the Python testrepository tool, maintaining
//! complete on-disk format compatibility with the original.

pub mod error;
pub mod repository;
pub mod commands;
pub mod ui;

pub use error::{Error, Result};
