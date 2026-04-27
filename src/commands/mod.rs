//! Command system for inquest
//!
//! Commands are discovered and executed through the Command trait.

use crate::error::Result;
use crate::ui::UI;

pub mod analyze_isolation;
pub mod auto;
pub mod config;
pub mod diff;
pub mod export;
pub mod failing;
pub mod flaky;
pub mod help;
pub mod info;
pub mod init;
pub mod last;
pub mod list_tests;
pub mod load;
pub mod log;
pub mod quickstart;
pub mod run;
pub mod running;
pub mod slowest;
pub mod stats;
#[cfg(feature = "testr")]
pub mod upgrade;
pub mod utils;
pub mod wait;

pub use analyze_isolation::AnalyzeIsolationCommand;
pub use auto::AutoCommand;
pub use config::ConfigCommand;
pub use diff::DiffCommand;
pub use export::{ExportCommand, ExportFormat};
pub use failing::FailingCommand;
pub use flaky::FlakyCommand;
pub use help::HelpCommand;
pub use info::InfoCommand;
pub use init::InitCommand;
pub use last::LastCommand;
pub use list_tests::ListTestsCommand;
pub use load::LoadCommand;
pub use log::LogCommand;
pub use quickstart::QuickstartCommand;
pub use run::RunCommand;
pub use running::RunningCommand;
pub use slowest::SlowestCommand;
pub use stats::StatsCommand;
#[cfg(feature = "testr")]
pub use upgrade::UpgradeCommand;
pub use wait::WaitCommand;

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
