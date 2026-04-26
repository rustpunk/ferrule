use clap::{Parser, Subcommand};

mod commands;
mod daemon;
mod error;
mod output;
mod repl;

use commands::{
    BookmarkArgs, ConnArgs, DescribeArgs, DumpArgs, ExplainArgs, LoadArgs, QueryArgs, ReplArgs,
    TablesArgs,
};
use error::CliError;

/// Ferrule — the collar that joins you to your data.
#[derive(Parser)]
#[command(name = "ferrule")]
#[command(version)]
#[command(about = "A Rust-native database query CLI")]
struct Cli {
    /// Path to config file
    #[arg(short, long, global = true)]
    config: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage saved connections
    #[command(alias = "conn")]
    Connection(ConnArgs),

    /// Manage query bookmarks
    Bookmark(BookmarkArgs),

    /// Explain a query execution plan
    Explain(ExplainArgs),

    /// Dump a table to CSV/JSON/SQL
    Dump(DumpArgs),

    /// Load data from CSV/JSON into a table
    Load(LoadArgs),

    /// Interactive REPL
    #[command(alias = "r")]
    Repl(ReplArgs),

    /// Execute a SQL query
    #[command(alias = "q")]
    Query(QueryArgs),

    /// List tables
    Tables(TablesArgs),

    /// Describe a table
    Describe(DescribeArgs),
}

fn run_daemon_mode() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(daemon::run_daemon_server())?;
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "__daemon" {
        if let Err(e) = run_daemon_mode() {
            eprintln!("Daemon error: {e}");
            std::process::exit(1);
        }
        return;
    }

    miette::set_hook(Box::new(|_| {
        Box::new(
            miette::MietteHandlerOpts::new()
                .terminal_links(true)
                .build(),
        )
    }))
    .ok();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let result: Result<(), CliError> = rt.block_on(async {
        let cli = Cli::parse();
        let global_config =
            ferrule_config::GlobalConfig::load(cli.config.as_deref()).unwrap_or_default();

        match cli.command {
            Commands::Connection(args) => commands::conn::run(args, &global_config).await,
            Commands::Query(args) => commands::query::run(args, &global_config).await,
            Commands::Bookmark(args) => commands::bookmark::run(args, &global_config).await,
            Commands::Explain(args) => commands::explain::run(args, &global_config).await,
            Commands::Repl(args) => commands::repl::run(args, &global_config).await,
            Commands::Dump(args) => commands::dump::run(args, &global_config).await,
            Commands::Load(args) => commands::load::run(args, &global_config).await,
            Commands::Tables(args) => commands::tables::run(args, &global_config).await,
            Commands::Describe(args) => commands::describe::run(args, &global_config).await,
        }
    });

    if let Err(err) = result {
        let code = err.exit_code();
        let report = miette::Report::new(err);
        eprintln!("{:?}", report);
        std::process::exit(code);
    }
}
