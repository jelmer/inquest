//! inq - Command-line tool for test repository management

use clap::builder::ValueHint;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use inquest::commands::*;
use inquest::config::{parse_duration_string, TimeoutSetting};
use inquest::error::Result;
use inquest::ui::UI;

// Explicit imports for commands not covered by wildcard
use inquest::commands::{AnalyzeIsolationCommand, BisectCommand};

#[derive(Parser)]
#[command(name = "inq")]
#[command(about = "Test repository management tool", long_about = None)]
#[command(version)]
#[command(disable_help_subcommand = true)]
struct Cli {
    /// Repository path (defaults to current directory)
    #[arg(short = 'C', long, global = true, value_hint = ValueHint::DirPath)]
    directory: Option<String>,

    /// Select a named profile from inquest.toml. Falls back to the
    /// `INQ_PROFILE` environment variable, then `default_profile`
    /// from the config file. Use "default" to force the base layer.
    #[arg(long, global = true, value_name = "NAME")]
    profile: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
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

        /// Output format: json, junit, tap, or github
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

    /// Print the resolved effective configuration
    Config {
        /// Per-test timeout: "5m", "auto", or "disabled"
        #[arg(long, value_name = "TIMEOUT")]
        test_timeout: Option<String>,

        /// Overall run timeout: "30m", "auto", or "disabled"
        #[arg(long, value_name = "DURATION")]
        max_duration: Option<String>,

        /// Kill test process if no output for this duration (e.g. "60s")
        #[arg(long, value_name = "DURATION")]
        no_output_timeout: Option<String>,

        /// Test ordering strategy
        #[arg(long, value_name = "ORDER")]
        order: Option<String>,

        /// Number of parallel test workers
        #[arg(long, short = 'j', value_name = "N", alias = "concurrency", num_args = 0..=1, default_missing_value = "0")]
        parallel: Option<usize>,

        /// List defined profile names from the config file and exit
        #[arg(long)]
        list_profiles: bool,
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

    /// Re-run exactly the tests of a previous run, in the same order with the same args
    Rerun {
        /// Run ID to re-run (defaults to latest; supports negative indices like -1, -2)
        #[arg(value_hint = ValueHint::Other)]
        run: Option<String>,
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

    /// Show flakiest tests across recorded runs
    Flaky {
        /// Number of tests to show
        #[arg(short = 'n', long, default_value = "10", conflicts_with = "all")]
        count: usize,

        /// Show all candidate tests (not just top N)
        #[arg(long)]
        all: bool,

        /// Minimum number of recorded runs a test must appear in to be ranked
        #[arg(long, default_value = "5")]
        min_runs: usize,
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

    /// Drop older test runs from the repository
    Prune {
        /// Keep the N most recent runs and prune the rest
        #[arg(long, value_name = "N", conflicts_with_all = ["older_than", "run", "all"])]
        keep: Option<usize>,

        /// Prune runs older than the given duration (e.g. "30d", "2w", "1h")
        #[arg(long = "older-than", value_name = "DURATION", conflicts_with_all = ["run", "all"])]
        older_than: Option<String>,

        /// Prune the named run by ID. May be given multiple times.
        #[arg(long = "run", value_name = "ID", conflicts_with = "all")]
        run: Vec<String>,

        /// Prune every run in the repository
        #[arg(long)]
        all: bool,

        /// Show what would be pruned without modifying the repository
        #[arg(long)]
        dry_run: bool,
    },

    /// Analyze test isolation issues using bisection
    #[command(name = "analyze-isolation")]
    AnalyzeIsolation {
        /// The test to analyze for isolation issues
        test: String,
    },

    /// Bisect git history to find the commit that broke a test
    Bisect {
        /// The test to bisect
        test: String,

        /// Known-good commit (defaults to the most recent recorded run where
        /// the test passed)
        #[arg(long, value_name = "COMMIT")]
        good: Option<String>,

        /// Known-bad commit (defaults to the most recent recorded run where
        /// the test failed, or HEAD if none)
        #[arg(long, value_name = "COMMIT")]
        bad: Option<String>,
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

        /// Restrict the post-run summary counts to results carrying the given
        /// tag. Repeat to allow several tags. Prefix with `!` to exclude
        /// results carrying the tag (e.g. `--tag worker-0 --tag '!slow'`).
        /// Overrides `filter_tags` from the config file.
        #[arg(long = "tag", value_name = "TAG")]
        filter_tags: Vec<String>,

        /// Run only tests whose ID starts with this prefix. Dotted segments
        /// may be abbreviated to single letters when the expansion against
        /// the discovered test list is unique (e.g. "bt.test_foo" expanding
        /// to "breezy.tests.test_foo"). May be given multiple times.
        #[arg(long = "starting-with", short = 's', value_name = "TESTID")]
        starting_with: Vec<String>,

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

        /// Test ordering: "auto" (smart pick from history), "discovery",
        /// "alphabetical", "failing-first", "spread", "shuffle[:<seed>]",
        /// "slowest-first", "fastest-first", or "frequent-failing-first"
        #[arg(long, value_name = "ORDER")]
        order: Option<String>,

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

    // CLI flag wins over the env var; the config file's `default_profile`
    // is only consulted when neither is set, and that lookup happens
    // inside `ConfigFile::resolve`.
    let profile = cli
        .profile
        .clone()
        .or_else(|| std::env::var(inquest::config::PROFILE_ENV_VAR).ok());

    let command = cli.command.unwrap_or(Commands::Run {
        failing: false,
        force_init: false,
        auto: true,
        partial: false,
        load_list: None,
        parallel: None,
        until_failure: false,
        max_iterations: None,
        isolated: false,
        subunit: false,
        all_output: false,
        testfilters: Vec::new(),
        filter_tags: Vec::new(),
        starting_with: Vec::new(),
        test_timeout: None,
        max_duration: None,
        no_output_timeout: None,
        max_restarts: None,
        // Bare `inq` picks an ordering automatically: frequent-failing-first
        // when there's failure history, else spread. Explicit `inq run`
        // keeps Discovery as its default.
        order: Some("auto".to_string()),
        testargs: Vec::new(),
    });

    let result = match command {
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
        Commands::Config {
            test_timeout,
            max_duration,
            no_output_timeout,
            order,
            parallel,
            list_profiles,
        } => {
            let cmd = ConfigCommand {
                base_path: cli.directory,
                test_timeout,
                max_duration,
                no_output_timeout,
                order,
                concurrency: parallel,
                profile: profile.clone(),
                list_profiles,
            };
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
        Commands::Rerun { run } => {
            let cmd = RerunCommand::new(cli.directory, run);
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
        Commands::Flaky {
            count,
            all,
            min_runs,
        } => {
            let display_count = if all { usize::MAX } else { count };
            let cmd = FlakyCommand::new(cli.directory, display_count, min_runs);
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
        Commands::Prune {
            keep,
            older_than,
            run,
            all,
            dry_run,
        } => {
            let selection = match (keep, older_than, run.is_empty(), all) {
                (Some(n), None, true, false) => PruneSelection::Keep(n),
                (None, Some(s), true, false) => {
                    match inquest::commands::prune::parse_age_string(&s) {
                        Ok(d) => PruneSelection::OlderThan(d),
                        Err(e) => {
                            tracing::error!("{}", e);
                            std::process::exit(1);
                        }
                    }
                }
                (None, None, false, false) => PruneSelection::Explicit(run),
                (None, None, true, true) => PruneSelection::All,
                (None, None, true, false) => {
                    tracing::error!("specify one of --keep, --older-than, --run, or --all");
                    std::process::exit(2);
                }
                _ => unreachable!("clap conflicts_with_all should reject combinations"),
            };
            let cmd = PruneCommand::new(cli.directory, selection, dry_run);
            cmd.execute(&mut ui)
        }
        Commands::AnalyzeIsolation { test } => {
            let cmd = AnalyzeIsolationCommand::new(cli.directory, test);
            cmd.execute(&mut ui)
        }
        Commands::Bisect { test, good, bad } => {
            let cmd = BisectCommand::new(cli.directory, test)
                .with_good_commit(good)
                .with_bad_commit(bad);
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
            order,
            testfilters,
            filter_tags,
            starting_with,
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
            let test_order = match order {
                Some(s) => match s.parse::<inquest::ordering::TestOrder>() {
                    Ok(o) => Some(o),
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
                filter_tags: if filter_tags.is_empty() {
                    None
                } else {
                    Some(filter_tags)
                },
                starting_with: if starting_with.is_empty() {
                    None
                } else {
                    Some(starting_with)
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
                test_order,
                stderr_capture: None,
                run_id_slot: None,
                cancellation_token: None,
                test_ids_override: None,
                profile: profile.clone(),
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
