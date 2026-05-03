//! `ferrule migrate` — schema migration runner.

use super::{resolve_connection, ConnectionFlags};
use crate::error::CliError;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::connection::ConnectOptions;
use ferrule_core::migrate::{Direction, MigrationEngine};
use std::path::PathBuf;

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct MigrateArgs {
    /// Connection name or raw URL
    pub connection: String,

    /// Connection password
    #[arg(short = 'p', long)]
    pub password: Option<String>,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,

    /// Directory containing `.up.sql` / `.down.sql` files.
    #[arg(short, long, value_name = "DIR", default_value = "migrations")]
    pub dir: PathBuf,

    #[clap(subcommand)]
    pub cmd: MigrateCmd,
}

#[derive(Subcommand)]
pub enum MigrateCmd {
    /// Apply all pending `.up.sql` migrations in order.
    Up,

    /// Roll back the most recently applied migration.
    Down,

    /// Show applied / pending / drift summary.
    Status,

    /// List all applied migrations (most recent first).
    History,

    /// Verify checksums for every applied migration.
    Verify,

    /// Create a new up/down migration pair.
    Create {
        /// Descriptive name (snake_case recommended).
        name: String,
    },
}

fn core_err(msg: String) -> CliError {
    CliError::query(ferrule_core::CoreError::QueryFailed(msg))
}

pub async fn run(args: MigrateArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    let resolved = resolve_connection(
        &args.connection,
        args.password,
        args.conn_flags.ssh_tunnel.as_deref(),
        args.conn_flags.ssh_key.as_deref(),
        args.conn_flags.proxy_url.as_deref(),
        global_config,
    )
    .await?;

    let opts = ConnectOptions {
        insecure: args.conn_flags.insecure,
    };
    let conn = super::connect_resolved(resolved, &opts).await?;

    let mut engine = MigrationEngine::new(conn, args.dir.clone());

    match args.cmd {
        MigrateCmd::Up => {
            engine
                .ensure_migration_table()
                .await
                .map_err(CliError::query)?;
            let pending = engine.pending_migrations().await.map_err(CliError::query)?;
            if pending.is_empty() {
                println!("No pending migrations.");
                return Ok(());
            }
            println!("Applying {} migration(s)...", pending.len());
            for m in pending {
                println!("  ↑ {}", m.version);
                engine.apply_up(&m).await.map_err(CliError::query)?;
            }
            println!("Done.");
        }

        MigrateCmd::Down => {
            engine
                .ensure_migration_table()
                .await
                .map_err(CliError::query)?;
            let applied = engine.last_applied(1).await.map_err(CliError::query)?;
            let Some(last) = applied.into_iter().next() else {
                return Err(core_err("no migrations to roll back".into()));
            };
            let files = engine.scan_dir(Direction::Down).map_err(CliError::query)?;
            let Some(file) = files.into_iter().find(|f| f.version == last.version) else {
                return Err(core_err(format!(
                    "down migration not found for version {}",
                    last.version
                )));
            };
            println!("  ↓ {}", file.version);
            engine.apply_down(&file).await.map_err(CliError::query)?;
            println!("Rolled back {}.", file.version);
        }

        MigrateCmd::Status => {
            engine
                .ensure_migration_table()
                .await
                .map_err(CliError::query)?;
            let applied = engine.applied_versions().await.map_err(CliError::query)?;
            let up_files = engine.scan_dir(Direction::Up).map_err(CliError::query)?;
            let applied_count = up_files
                .iter()
                .filter(|f| applied.contains(&f.version))
                .count();
            let pending_count = up_files.len() - applied_count;

            for f in up_files {
                let marker = if applied.contains(&f.version) {
                    "✔"
                } else {
                    "○"
                };
                println!("{} {}", marker, f.version);
            }
            println!();
            println!("{} applied, {} pending", applied_count, pending_count);
        }

        MigrateCmd::History => {
            engine
                .ensure_migration_table()
                .await
                .map_err(CliError::query)?;
            let applied = engine.last_applied(100).await.map_err(CliError::query)?;
            if applied.is_empty() {
                println!("No applied migrations.");
            }
            for m in applied {
                println!("{} {}", m.version, &m.checksum[..16.min(m.checksum.len())]);
            }
        }

        MigrateCmd::Verify => {
            engine
                .ensure_migration_table()
                .await
                .map_err(CliError::query)?;
            let applied = engine.last_applied(100).await.map_err(CliError::query)?;
            let mut ok = true;
            for m in applied {
                match engine.verify_checksum(&m.version).await {
                    Err(e) => {
                        eprintln!("  ✗ {} — {}", m.version, e);
                        ok = false;
                    }
                    Ok(()) => println!("  ✔ {}", m.version),
                }
            }
            if !ok {
                std::process::exit(1);
            }
        }

        MigrateCmd::Create { name } => {
            let ts = chrono::Utc::now().format("%Y%m%d%H%M%S");
            let stem = format!("{}_{}", ts, name);
            let up = args.dir.join(format!("{}.up.sql", stem));
            let down = args.dir.join(format!("{}.down.sql", stem));
            tokio::fs::create_dir_all(&args.dir)
                .await
                .map_err(|e| core_err(format!("cannot create migrations dir: {}", e)))?;
            tokio::fs::write(&up, "-- up\n\n")
                .await
                .map_err(|e| core_err(format!("cannot write {}: {}", up.display(), e)))?;
            tokio::fs::write(&down, "-- down\n\n")
                .await
                .map_err(|e| core_err(format!("cannot write {}: {}", down.display(), e)))?;
            println!("Created migrations/{}_*.{{up,down}}.sql", ts);
        }
    }

    Ok(())
}
