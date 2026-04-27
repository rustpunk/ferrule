use super::{
    check_daemon_ssh_compat, connect_resolved, resolve_connection, ConnectionFlags, OutputFlags,
};
use crate::error::CliError;
use clap::{Args, Subcommand};
use ferrule_config::bookmarks::BookmarkStore;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::backend::Backend;
use ferrule_core::connection::{ConnectOptions, StatementResult};
use ferrule_core::formatter::format_result;

#[derive(Args, Clone, Debug)]
pub struct BookmarkArgs {
    #[command(subcommand)]
    pub command: BookmarkCommand,
}

#[derive(Subcommand, Clone, Debug)]
pub enum BookmarkCommand {
    /// Save a new bookmark
    Add {
        /// Bookmark name (may be dotted, e.g. pg.select_users)
        name: String,
        /// SQL statement
        sql: String,
        /// Optional connection hint
        #[arg(short, long)]
        connection: Option<String>,
    },
    /// List all bookmarks
    List,
    /// Run a bookmark
    Run {
        /// Bookmark name
        name: String,
        /// Positional parameters (${1}, ${2}, ...)
        params: Vec<String>,
        /// Explicit connection override
        #[arg(short, long)]
        connection: Option<String>,
        #[command(flatten)]
        output: OutputFlags,
        #[command(flatten)]
        conn_flags: ConnectionFlags,
    },
    /// Delete a bookmark
    Delete {
        /// Bookmark name
        name: String,
    },
}

pub async fn run(args: BookmarkArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    match args.command {
        BookmarkCommand::Add {
            name,
            sql,
            connection,
        } => cmd_add(name, sql, connection).await,
        BookmarkCommand::List => cmd_list(),
        BookmarkCommand::Run {
            name,
            params,
            connection,
            output,
            conn_flags,
        } => cmd_run(name, params, connection, output, conn_flags, global_config).await,
        BookmarkCommand::Delete { name } => cmd_delete(name).await,
    }
}

async fn cmd_add(name: String, sql: String, connection: Option<String>) -> Result<(), CliError> {
    let mut store = BookmarkStore::load().map_err(CliError::registry)?;
    store.insert(name.clone(), sql, connection);
    store.save().map_err(CliError::registry)?;
    println!("Bookmark '{}' added.", name);
    Ok(())
}

fn cmd_list() -> Result<(), CliError> {
    let store = BookmarkStore::load().map_err(CliError::registry)?;
    let entries = store.list();
    if entries.is_empty() {
        println!("No bookmarks saved.");
        return Ok(());
    }
    let max_width = entries.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    println!("{:width$} | SQL", "Name", width = max_width);
    println!("{}", "-".repeat(max_width + 3 + 60));
    for (name, bm) in entries {
        let truncated = if bm.sql.len() > 60 {
            format!("{}...", &bm.sql[..57])
        } else {
            bm.sql.clone()
        };
        println!("{:width$} | {}", name, truncated, width = max_width);
    }
    Ok(())
}

async fn cmd_run(
    name: String,
    params: Vec<String>,
    explicit_connection: Option<String>,
    output: OutputFlags,
    conn_flags: ConnectionFlags,
    global_config: &GlobalConfig,
) -> Result<(), CliError> {
    let store = BookmarkStore::load().map_err(CliError::registry)?;
    let bookmark = store
        .get(&name)
        .ok_or_else(|| CliError::usage(format!("Bookmark '{}' not found.", name)))?;

    let sql = BookmarkStore::resolve_params(&bookmark.sql, &params);
    if sql != bookmark.sql {
        eprintln!("[ferrule] resolved SQL: {}", sql);
    }

    // Resolve connection string
    let connection_str = if let Some(conn) = explicit_connection {
        conn
    } else if let Some(conn) = bookmark.connection.as_deref() {
        conn.to_string()
    } else if let Some(hint) = BookmarkStore::connection_hint(&name) {
        hint.to_string()
    } else if let Some(profile) = global_config.connection.get("default") {
        profile.url.clone()
    } else {
        return Err(CliError::usage(
            "No connection provided. Use --connection or set a connection hint in the bookmark name.",
        ));
    };

    let format = output.resolve_format(global_config);
    let limit = output.resolve_limit(global_config);
    let offset = output.offset;

    let resolved = resolve_connection(
        &connection_str,
        None,
        conn_flags.ssh_tunnel.as_deref(),
        conn_flags.ssh_key.as_deref(),
        global_config,
    )
    .await?;
    check_daemon_ssh_compat(conn_flags.daemon, &resolved)?;

    if conn_flags.daemon {
        eprintln!("[ferrule] Routing via daemon...");
        let payload = crate::daemon::daemon_query(
            &sql,
            &resolved.url,
            conn_flags.insecure,
            format,
            limit,
            offset,
        )
        .await?;
        println!("{}", payload);
        return Ok(());
    }

    let opts = ConnectOptions {
        insecure: conn_flags.insecure,
    };
    if opts.insecure {
        eprintln!("Warning: --insecure disables TLS certificate verification.");
    }

    let backend = Backend::from_scheme(resolved.url.scheme()).ok_or_else(|| {
        CliError::usage(format!("Unsupported scheme: {}", resolved.url.scheme()))
    })?;

    let mut conn = connect_resolved(resolved, &opts).await?;

    let sql = ferrule_core::apply_paging(&sql, limit, offset, backend).map_err(CliError::query)?;

    let results = match conn.query(&sql).await {
        Ok(qr) => vec![StatementResult::Query(qr)],
        Err(ferrule_core::CoreError::QueryFailed(_)) => match conn.execute(&sql).await {
            Ok(summary) => vec![StatementResult::Summary(summary)],
            Err(_) => conn.execute_multi(&sql).await.map_err(CliError::query)?,
        },
        Err(e) => return Err(CliError::query(e)),
    };

    if results.len() == 1 {
        let text = render_single_result(&results[0], format, limit, offset)?;
        match &results[0] {
            StatementResult::Query(_) => println!("{}", text),
            StatementResult::Summary(_) => eprintln!("{}", text),
        }
    } else {
        for (i, result) in results.iter().enumerate() {
            match result {
                StatementResult::Query(_) => {
                    let rendered = render_single_result(result, format, limit, offset)?;
                    println!("-- Result set {}\n", i + 1);
                    println!("{}", rendered);
                    println!();
                }
                StatementResult::Summary(s) => {
                    eprintln!(
                        "-- Statement {}: {} rows affected\n",
                        i + 1,
                        s.rows_affected.unwrap_or(0)
                    );
                }
            }
        }
    }

    Ok(())
}

async fn cmd_delete(name: String) -> Result<(), CliError> {
    let mut store = BookmarkStore::load().map_err(CliError::registry)?;
    store.remove(&name).map_err(CliError::registry)?;
    store.save().map_err(CliError::registry)?;
    println!("Bookmark '{}' deleted.", name);
    Ok(())
}

fn render_single_result(
    result: &StatementResult,
    format: ferrule_core::formatter::OutputFormat,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<String, CliError> {
    match result {
        StatementResult::Query(qr) => {
            let mut qr = qr.clone();
            if let Some(off) = offset {
                if off >= qr.rows.len() {
                    qr.rows.clear();
                } else {
                    qr.rows = qr.rows.split_off(off);
                }
            }
            if let Some(n) = limit {
                if qr.rows.len() > n {
                    qr.rows.truncate(n);
                }
            }
            format_result(&qr, format).map_err(CliError::query)
        }
        StatementResult::Summary(s) => {
            Ok(format!("{} rows affected", s.rows_affected.unwrap_or(0)))
        }
    }
}
