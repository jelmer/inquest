//! inq - Command-line tool for test repository management

use clap::builder::ValueHint;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use inquest::commands::*;
use inquest::config::{parse_duration_string, TimeoutSetting};
use inquest::error::Result;
use inquest::ui::UI;

// Explicit imports for commands not covered by wildcard
use inquest::commands::AnalyzeIsolationCommand;

#[derive(Parser)]
#[command(name = "inq")]
#[command(about = "Test repository management tool", long_about = None)]
#[command(version)]
#[command(disable_help_subcommand = true)]
struct Cli {
    /// Repository path (defaults to current directory)
    #[arg(short = 'C', long, global = true, value_hint = ValueHint::DirPath)]
    directory: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Auto-detect project type and generate inquest.toml
    Auto,

    /// Initialize a new test repository
    Init,

    /// Export test results in standard formats
    Export {
        /// Run ID to export (defaults to latest; supports negative indices like -1, -2)
        #[arg(long, short = 'r', value_hint = ValueHint::Other)]
        run: Option<String>,

        /// Output format: json, junit, or tap
        #[arg(long, short = 'f', default_value = "json")]
        format: String,
    },

    /// Compare two test runs and show what changed
    Diff {
        /// First run ID (defaults to second-to-latest; supports negative indices like -1, -2)
        #[arg(value_hint = ValueHint::Other)]
        run1: Option<String>,

        /// Second run ID (defaults to latest; supports negative indices like -1, -2)
        #[arg(value_hint = ValueHint::Other)]
        run2: Option<String>,
    },

    /// Show detailed information about a test run
    Info {
        /// Run ID to show (defaults to latest; supports negative indices like -1, -2)
        #[arg(long, short = 'r', value_hint = ValueHint::Other)]
        run: Option<String>,
    },

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

    /// Show results from a test run
    Last {
        /// Run ID to show (defaults to latest; supports negative indices like -1, -2)
        #[arg(long, short = 'r', value_hint = ValueHint::Other)]
        run: Option<String>,

        /// Show output as a subunit stream
        #[arg(long)]
        subunit: bool,

        /// Don't show test output/tracebacks for failed tests
        #[arg(long)]
        no_output: bool,
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

    /// Show currently in-progress test runs
    Running,

    /// Wait for in-progress test runs to complete
    Wait {
        /// Run ID to wait for (defaults to all running runs)
        #[arg(long, short = 'r', value_hint = ValueHint::Other)]
        run: Option<String>,

        /// Maximum time to wait in seconds
        #[arg(long, default_value_t = 600)]
        timeout: u64,

        /// Return early when any test matches the given status. Accepts
        /// "success", "failure", "error", "skip", "xfail", "uxsuccess",
        /// plus the aliases "failing" and "passing". Can be repeated.
        #[arg(long = "status", value_name = "STATUS")]
        status_filter: Vec<String>,

        /// Print each new test result as it is observed
        #[arg(long)]
        stream: bool,

        /// With --stream, only print failing tests
        #[arg(long, requires = "stream")]
        only_failures: bool,
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

    /// Show logs for individual tests
    Log {
        /// Run ID to show logs from (defaults to latest)
        #[arg(long, short = 'r', value_hint = ValueHint::Other)]
        run: Option<String>,

        /// Test ID patterns to match (glob-style wildcards)
        #[arg(value_name = "TESTPATTERN")]
        tests: Vec<String>,
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

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },

    /// Upgrade a .testrepository/ to .inquest/ format
    #[cfg(feature = "testr")]
    Upgrade,

    /// Start MCP (Model Context Protocol) server over stdio
    #[cfg(feature = "mcp")]
    Mcp,

    /// Run tests and load results
    Run {
        /// Run only the tests that failed in the last run
        #[arg(long)]
        failing: bool,

        /// Create repository if it doesn't exist
        #[arg(long)]
        force_init: bool,

        /// Auto-detect project type and generate inquest.toml if missing
        #[arg(long)]
        auto: bool,

        /// Partial run mode (update failing tests additively)
        #[arg(long)]
        partial: bool,

        /// Only run tests listed in the named file (one test ID per line)
        #[arg(long, value_hint = ValueHint::FilePath)]
        load_list: Option<String>,

        /// Run tests in parallel across multiple workers (defaults to number of CPUs if no value given)
        #[arg(long, short = 'j', value_name = "N", alias = "concurrency", num_args = 0..=1, default_missing_value = "0")]
        parallel: Option<usize>,

        /// Run tests repeatedly until they fail
        #[arg(long)]
        until_failure: bool,

        /// Maximum number of iterations for --until-failure
        #[arg(long, requires = "until_failure")]
        max_iterations: Option<usize>,

        /// Run each test in a separate process (completely isolated)
        #[arg(long)]
        isolated: bool,

        /// Show output as a subunit stream
        #[arg(long)]
        subunit: bool,

        /// Show output from all tests (by default only failed tests show output)
        #[arg(long)]
        all_output: bool,

        /// Test ID filters (regex patterns to filter which tests to run)
        #[arg(value_name = "TESTFILTER")]
        testfilters: Vec<String>,

        /// Per-test timeout: "5m", "auto" (from history), or "disabled" (default)
        #[arg(long, value_name = "TIMEOUT")]
        test_timeout: Option<String>,

        /// Overall run timeout: "30m", "auto" (from history), or "disabled" (default)
        #[arg(long, value_name = "DURATION")]
        max_duration: Option<String>,

        /// Kill test process if no output for this duration (e.g. "60s")
        #[arg(long, value_name = "DURATION")]
        no_output_timeout: Option<String>,

        /// Maximum number of test process restarts on timeout or crash
        #[arg(long, value_name = "N")]
        max_restarts: Option<usize>,

        /// Additional arguments to pass to the test command (use after --)
        #[arg(last = true, value_name = "TESTARGS")]
        testargs: Vec<String>,
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
        tracing::error!("{}", message);
        Ok(())
    }

    fn warning(&mut self, message: &str) -> Result<()> {
        tracing::warn!("{}", message);
        Ok(())
    }
}

fn main() {
    // Initialize tracing with stderr output, respecting RUST_LOG env var
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .init();

    let cli = Cli::parse();

    if let Some(ref dir) = cli.directory {
        let path = std::path::Path::new(dir);
        if !path.exists() {
            eprintln!("Error: directory does not exist: {}", dir);
            std::process::exit(1);
        }
        if !path.is_dir() {
            eprintln!("Error: not a directory: {}", dir);
            std::process::exit(1);
        }
    }

    let mut ui = CliUI;

    let result = match cli.command {
        Commands::Auto => {
            let cmd = AutoCommand::new(cli.directory);
            cmd.execute(&mut ui)
        }
        Commands::Init => {
            let cmd = InitCommand::new(cli.directory);
            cmd.execute(&mut ui)
        }
        Commands::Export { run, format } => {
            let format = match format.parse() {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("{}", e);
                    std::process::exit(1);
                }
            };
            let cmd = ExportCommand::new(cli.directory, run, format);
            cmd.execute(&mut ui)
        }
        Commands::Diff { run1, run2 } => {
            let cmd = DiffCommand::new(cli.directory, run1, run2);
            cmd.execute(&mut ui)
        }
        Commands::Info { run } => {
            let cmd = InfoCommand::new(cli.directory, run);
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
        Commands::Last {
            run,
            subunit,
            no_output,
        } => {
            let cmd = if subunit {
                LastCommand::with_subunit(cli.directory, run)
            } else if no_output {
                LastCommand::with_output_control(cli.directory, run, false)
            } else {
                LastCommand::with_run(cli.directory, run)
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
        Commands::Running => {
            let cmd = RunningCommand::new(cli.directory);
            cmd.execute(&mut ui)
        }
        Commands::Wait {
            run,
            timeout,
            status_filter,
            stream,
            only_failures,
        } => {
            let cmd = match WaitCommand::new(
                cli.directory,
                run,
                std::time::Duration::from_secs(timeout),
                status_filter,
                stream,
                only_failures,
            ) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("{}", e);
                    std::process::exit(2);
                }
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
        Commands::Log { run, tests } => {
            let patterns: Vec<glob::Pattern> = match tests
                .iter()
                .map(|t| glob::Pattern::new(t))
                .collect::<std::result::Result<Vec<_>, _>>()
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!("invalid pattern: {}", e);
                    std::process::exit(1);
                }
            };
            let cmd = LogCommand::new(cli.directory, run, patterns);
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
        Commands::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "inq", &mut std::io::stdout());
            Ok(0)
        }
        #[cfg(feature = "testr")]
        Commands::Upgrade => {
            let cmd = UpgradeCommand::new(cli.directory);
            cmd.execute(&mut ui)
        }
        #[cfg(feature = "mcp")]
        Commands::Mcp => {
            let directory = cli
                .directory
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
            let rt = tokio::runtime::Runtime::new().unwrap();
            match rt.block_on(inquest::mcp::serve(directory)) {
                Ok(()) => Ok(0),
                Err(e) => {
                    tracing::error!("MCP server error: {}", e);
                    Ok(1)
                }
            }
        }
        Commands::Run {
            failing,
            force_init,
            auto,
            partial,
            load_list,
            parallel,
            until_failure,
            max_iterations,
            isolated,
            subunit,
            all_output,
            test_timeout,
            max_duration,
            no_output_timeout,
            max_restarts,
            testfilters,
            testargs,
        } => {
            // Parse timeout settings: CLI flags override config file values.
            // Config file values are resolved later in RunCommand::execute() if these are Disabled/None.
            let test_timeout = match test_timeout {
                Some(s) => match TimeoutSetting::parse(&s) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!("{}", e);
                        std::process::exit(1);
                    }
                },
                None => TimeoutSetting::Disabled,
            };
            let max_duration = match max_duration {
                Some(s) => match TimeoutSetting::parse(&s) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!("{}", e);
                        std::process::exit(1);
                    }
                },
                None => TimeoutSetting::Disabled,
            };
            let no_output_timeout = match no_output_timeout {
                Some(s) => match parse_duration_string(&s) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        tracing::error!("{}", e);
                        std::process::exit(1);
                    }
                },
                None => None,
            };

            let cmd = RunCommand {
                base_path: cli.directory,
                partial: partial || failing, // --failing implies partial mode
                failing_only: failing,
                force_init,
                auto,
                load_list,
                concurrency: parallel,
                until_failure,
                max_iterations,
                isolated,
                subunit,
                all_output,
                test_filters: if testfilters.is_empty() {
                    None
                } else {
                    Some(testfilters)
                },
                test_args: if testargs.is_empty() {
                    None
                } else {
                    Some(testargs)
                },
                test_timeout,
                max_duration,
                no_output_timeout,
                max_restarts,
                stderr_capture: None,
            };
            cmd.execute(&mut ui)
        }
    };

    match result {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            tracing::error!("{}", e);
            if matches!(e, inquest::error::Error::RepositoryNotFound(_)) {
                tracing::info!(
                    "Hint: Run 'inq init', use '--force-init', or add an inquest.toml to create a repository automatically."
                );
            }
            std::process::exit(1);
        }
    }
}
