//! Command system for testrepository
//!
//! Commands are discovered and executed through the Command trait.

use crate::error::Result;
use crate::ui::UI;

pub mod failing;
pub mod init;
pub mod last;
pub mod list_tests;
pub mod load;
pub mod run;
pub mod slowest;
pub mod stats;
mod utils;

pub use failing::FailingCommand;
pub use init::InitCommand;
pub use last::LastCommand;
pub use list_tests::ListTestsCommand;
pub use load::LoadCommand;
pub use run::RunCommand;
pub use slowest::SlowestCommand;
pub use stats::StatsCommand;

/// Trait that all commands must implement
pub trait Command {
    /// Execute the command
    fn execute(&self, ui: &mut dyn UI) -> Result<i32>;

    /// Get the command name
    fn name(&self) -> &str;

    /// Get command help text
    fn help(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockCommand;

    impl Command for MockCommand {
        fn execute(&self, _ui: &mut dyn UI) -> Result<i32> {
            Ok(0)
        }

        fn name(&self) -> &str {
            "mock"
        }

        fn help(&self) -> &str {
            "A mock command for testing"
        }
    }

    #[test]
    fn test_command_trait() {
        let cmd = MockCommand;
        assert_eq!(cmd.name(), "mock");
        assert_eq!(cmd.help(), "A mock command for testing");
    }
}
