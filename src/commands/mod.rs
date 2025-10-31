//! Command system for testrepository
//!
//! Commands are discovered and executed through the Command trait.

use crate::error::Result;
use crate::ui::UI;

pub mod init;
pub mod load;
pub mod last;
pub mod failing;

pub use init::InitCommand;
pub use load::LoadCommand;
pub use last::LastCommand;
pub use failing::FailingCommand;

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
