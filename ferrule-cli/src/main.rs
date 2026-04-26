use clap::{Parser, Subcommand};

mod commands;
mod error;
mod output;

use commands::{ConnArgs, DescribeArgs, QueryArgs, TablesArgs};
use error::CliError;

/// Ferrule — the collar that joins you to your data.
#[derive(Parser)]
#[command(name = "ferrule")]
#[command(version)]
#[command(about = "A Rust-native database query CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage saved connections
    #[command(alias = "conn")]
    Connection(ConnArgs),

    /// Execute a SQL query
    #[command(alias = "q")]
    Query(QueryArgs),

    /// List tables
    Tables(TablesArgs),

    /// Describe a table
    Describe(DescribeArgs),
}

fn main() {
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
        match cli.command {
            Commands::Connection(args) => commands::conn::run(args).await,
            Commands::Query(args) => commands::query::run(args).await,
            Commands::Tables(args) => commands::tables::run(args).await,
            Commands::Describe(args) => commands::describe::run(args).await,
        }
    });

    if let Err(err) = result {
        let code = err.exit_code();
        let report = miette::Report::new(err);
        eprintln!("{:?}", report);
        std::process::exit(code);
    }
}
