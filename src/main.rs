//! testr - Command-line tool for test repository management

use clap::{Parser, Subcommand};
use std::io::Write;
use testrepository::commands::*;
use testrepository::error::Result;
use testrepository::ui::UI;

#[derive(Parser)]
#[command(name = "testr")]
#[command(about = "Test repository management tool", long_about = None)]
struct Cli {
    /// Repository path (defaults to current directory)
    #[arg(short = 'C', long, global = true)]
    directory: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new test repository
    Init,

    /// Load test results from stdin
    Load,

    /// Show results from the last test run
    Last,

    /// Show failing tests from the last run
    Failing,

    /// Show repository statistics
    Stats,

    /// Show the slowest tests from the last run
    #[command(name = "slowest")]
    Slowest {
        /// Number of tests to show
        #[arg(short = 'n', long, default_value = "10")]
        count: usize,
    },

    /// List all available tests
    #[command(name = "list-tests")]
    ListTests,

    /// Run tests and load results
    Run {
        /// Run only the tests that failed in the last run
        #[arg(long)]
        failing: bool,
    },
}

/// Simple UI implementation that writes to stdout/stderr
struct CliUI;

impl UI for CliUI {
    fn output(&mut self, message: &str) -> Result<()> {
        println!("{}", message);
        Ok(())
    }

    fn error(&mut self, message: &str) -> Result<()> {
        eprintln!("Error: {}", message);
        Ok(())
    }

    fn warning(&mut self, message: &str) -> Result<()> {
        eprintln!("Warning: {}", message);
        Ok(())
    }
}

fn main() {
    let cli = Cli::parse();

    let mut ui = CliUI;

    let result = match cli.command {
        Commands::Init => {
            let cmd = InitCommand::new(cli.directory);
            cmd.execute(&mut ui)
        }
        Commands::Load => {
            let cmd = LoadCommand::new(cli.directory);
            cmd.execute(&mut ui)
        }
        Commands::Last => {
            let cmd = LastCommand::new(cli.directory);
            cmd.execute(&mut ui)
        }
        Commands::Failing => {
            let cmd = FailingCommand::new(cli.directory);
            cmd.execute(&mut ui)
        }
        Commands::Stats => {
            let cmd = StatsCommand::new(cli.directory);
            cmd.execute(&mut ui)
        }
        Commands::Slowest { count } => {
            let cmd = SlowestCommand::with_count(cli.directory, count);
            cmd.execute(&mut ui)
        }
        Commands::ListTests => {
            let cmd = ListTestsCommand::new(cli.directory);
            cmd.execute(&mut ui)
        }
        Commands::Run { failing } => {
            let cmd = if failing {
                RunCommand::with_failing_only(cli.directory)
            } else {
                RunCommand::new(cli.directory)
            };
            cmd.execute(&mut ui)
        }
    };

    match result {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "Error: {}", e);
            std::process::exit(1);
        }
    }
}
