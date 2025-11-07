//! testr - Command-line tool for test repository management

use clap::{Parser, Subcommand};
use std::io::Write;
use testrepository::commands::*;
use testrepository::error::Result;
use testrepository::ui::UI;

// Explicit imports for commands not covered by wildcard
use testrepository::commands::AnalyzeIsolationCommand;

#[derive(Parser)]
#[command(name = "testr")]
#[command(about = "Test repository management tool", long_about = None)]
#[command(version)]
#[command(disable_help_subcommand = true)]
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

    /// Show help information for commands
    Help {
        /// Command to show help for
        command: Option<String>,
    },

    /// Show quickstart documentation
    Quickstart,

    /// Load test results from stdin
    Load {
        /// Create repository if it doesn't exist
        #[arg(long)]
        force_init: bool,

        /// Partial run mode (update failing tests additively)
        #[arg(long)]
        partial: bool,
    },

    /// Show results from the last test run
    Last {
        /// Show output as a subunit stream
        #[arg(long)]
        subunit: bool,
    },

    /// Show failing tests from the last run
    Failing {
        /// List test IDs only, one per line (for scripting)
        #[arg(long)]
        list: bool,

        /// Show output as a subunit stream
        #[arg(long)]
        subunit: bool,
    },

    /// Show repository statistics
    Stats,

    /// Show the slowest tests from the last run
    #[command(name = "slowest")]
    Slowest {
        /// Number of tests to show
        #[arg(short = 'n', long, default_value = "10", conflicts_with = "all")]
        count: usize,

        /// Show all tests (not just top N)
        #[arg(long)]
        all: bool,
    },

    /// List all available tests
    #[command(name = "list-tests")]
    ListTests,

    /// Analyze test isolation issues using bisection
    #[command(name = "analyze-isolation")]
    AnalyzeIsolation {
        /// The test to analyze for isolation issues
        test: String,
    },

    /// Run tests and load results
    Run {
        /// Run only the tests that failed in the last run
        #[arg(long)]
        failing: bool,

        /// Create repository if it doesn't exist
        #[arg(long)]
        force_init: bool,

        /// Partial run mode (update failing tests additively)
        #[arg(long)]
        partial: bool,

        /// Only run tests listed in the named file (one test ID per line)
        #[arg(long)]
        load_list: Option<String>,

        /// Run tests in parallel across multiple workers
        #[arg(long, short = 'j', value_name = "N", alias = "concurrency")]
        parallel: Option<usize>,

        /// Run tests repeatedly until they fail
        #[arg(long)]
        until_failure: bool,

        /// Run each test in a separate process (completely isolated)
        #[arg(long)]
        isolated: bool,
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
        Commands::Help { command } => {
            let cmd = HelpCommand::new(command);
            cmd.execute(&mut ui)
        }
        Commands::Quickstart => {
            let cmd = QuickstartCommand::new();
            cmd.execute(&mut ui)
        }
        Commands::Load {
            force_init,
            partial,
        } => {
            let cmd = LoadCommand::with_partial(cli.directory, partial, force_init);
            cmd.execute(&mut ui)
        }
        Commands::Last { subunit } => {
            let cmd = if subunit {
                LastCommand::with_subunit(cli.directory)
            } else {
                LastCommand::new(cli.directory)
            };
            cmd.execute(&mut ui)
        }
        Commands::Failing { list, subunit } => {
            let cmd = if subunit {
                FailingCommand::with_subunit(cli.directory)
            } else if list {
                FailingCommand::with_list_only(cli.directory)
            } else {
                FailingCommand::new(cli.directory)
            };
            cmd.execute(&mut ui)
        }
        Commands::Stats => {
            let cmd = StatsCommand::new(cli.directory);
            cmd.execute(&mut ui)
        }
        Commands::Slowest { count, all } => {
            let display_count = if all { usize::MAX } else { count };
            let cmd = SlowestCommand::with_count(cli.directory, display_count);
            cmd.execute(&mut ui)
        }
        Commands::ListTests => {
            let cmd = ListTestsCommand::new(cli.directory);
            cmd.execute(&mut ui)
        }
        Commands::AnalyzeIsolation { test } => {
            let cmd = AnalyzeIsolationCommand::new(cli.directory, test);
            cmd.execute(&mut ui)
        }
        Commands::Run {
            failing,
            force_init,
            partial,
            load_list,
            parallel,
            until_failure,
            isolated,
        } => {
            let cmd = RunCommand::with_all_options(
                cli.directory,
                partial,
                failing,
                force_init,
                load_list,
                parallel,
                until_failure,
                isolated,
            );
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
