//! `ferrule migrate` — schema migration runner.

use super::{resolve_connection, ConnectionFlags};
use crate::error::CliError;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::connection::ConnectOptions;
use ferrule_core::migrate::{Dialect, Direction, MigrationEngine};
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

    // Capture the SQL dialect from the URL scheme before
    // `connect_resolved` consumes `resolved`. Unrecognised schemes fall
    // back to SQLite semantics (ANSI `LIMIT`, `TEXT` columns).
    let dialect = Dialect::from_scheme(resolved.url.scheme()).unwrap_or(Dialect::Sqlite);

    let conn = super::connect_resolved(resolved, &opts).await?;

    let mut engine = MigrationEngine::new(conn, args.dir.clone(), dialect);

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
            // Scan the migrations directory once and compare every applied
            // migration against the checksums already returned above — no
            // per-migration directory re-scan or follow-up SELECT.
            let drift = engine
                .verify_applied(&applied)
                .await
                .map_err(CliError::query)?;
            let drifted: std::collections::HashSet<&str> =
                drift.iter().map(|d| d.version.as_str()).collect();
            for m in &applied {
                if !drifted.contains(m.version.as_str()) {
                    println!("  ✔ {}", m.version);
                }
            }
            for d in &drift {
                eprintln!("  ✗ {} — {}", d.version, d.reason);
            }
            if !drift.is_empty() {
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
