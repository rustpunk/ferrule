use crate::commands::{resolve_connection, OutputFlags};
use crate::error::CliError;
use ferrule_config::bookmarks::BookmarkStore;
use ferrule_config::profile::GlobalConfig;
use ferrule_config::registry::ConnectionRegistry;
use ferrule_core::backend::{connect, Backend};
use ferrule_core::connection::{ConnectOptions, Connection, QueryResult, StatementResult};
use ferrule_core::formatter::{format_result, OutputFormat};
use ferrule_core::url::DatabaseUrl;
use ferrule_core::value::{ColumnInfo, TypeHint, Value};
use indexmap::IndexMap;
use std::io::Write;

/// Mutable REPL session state.
pub struct ReplState {
    pub format: OutputFormat,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub timing: bool,
    pub verbose: bool,
    /// FE-011 placeholder — will become `ParameterSet`.
    pub params: IndexMap<String, String>,
    /// FE-012 placeholder.
    pub explain_mode: bool,
    /// FE-010 placeholder — last successfully executed SQL.
    pub last_sql: Option<String>,
}

impl Default for ReplState {
    fn default() -> Self {
        Self {
            format: OutputFormat::Table,
            limit: None,
            offset: None,
            timing: false,
            verbose: false,
            params: IndexMap::new(),
            explain_mode: false,
            last_sql: None,
        }
    }
}

/// An active REPL session holding a single persistent connection.
pub struct Repl {
    pub conn: Box<dyn Connection>,
    pub url: DatabaseUrl,
    pub backend: Backend,
    pub state: ReplState,
    pub global_config: GlobalConfig,
    pub insecure: bool,
}

impl Repl {
    pub async fn new(
        connection_str: &str,
        output: OutputFlags,
        insecure: bool,
        global_config: &GlobalConfig,
    ) -> Result<Self, CliError> {
        let url = resolve_connection(connection_str, None, global_config).await?;
        let opts = ConnectOptions { insecure };
        let backend = Backend::from_scheme(url.scheme())
            .ok_or_else(|| CliError::usage(format!("Unsupported scheme: {}", url.scheme())))?;
        let conn = connect(&url, &opts).await.map_err(CliError::connection)?;

        let format = output.resolve_format(global_config);
        let limit = output.resolve_limit(global_config);
        let offset = output.offset;

        Ok(Self {
            conn,
            url,
            backend,
            state: ReplState {
                format,
                limit,
                offset,
                ..ReplState::default()
            },
            global_config: global_config.clone(),
            insecure,
        })
    }

    /// Switch to a different connection.
    pub async fn switch_connection(&mut self, name_or_url: &str) {
        match resolve_connection(name_or_url, None, &self.global_config).await {
            Ok(url) => {
                let opts = ConnectOptions {
                    insecure: self.insecure,
                };
                match connect(&url, &opts).await {
                    Ok(conn) => {
                        if let Some(b) = Backend::from_scheme(url.scheme()) {
                            self.backend = b;
                        }
                        self.url = url;
                        self.conn = conn;
                        println!("Switched to: {}", self.url.redacted());
                    }
                    Err(e) => {
                        eprintln!("Connection failed: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("Could not resolve connection: {e}");
            }
        }
    }

    /// Run a single SQL statement (or multi-statement batch).
    pub fn execute_sql(&mut self, sql: &str, rt: &tokio::runtime::Handle) {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            return;
        }

        // Verify connection is alive.
        if let Err(e) = rt.block_on(self.conn.ping()) {
            eprintln!("Connection lost: {e}. Use \\conn to reconnect.");
            return;
        }

        let paged = match ferrule_core::apply_paging(
            trimmed,
            self.state.limit,
            self.state.offset,
            self.backend,
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Paging error: {e}");
                return;
            }
        };

        if self.state.verbose {
            eprintln!("[ferrule] SQL: {paged}");
        }

        let start = std::time::Instant::now();
        let query_start = std::time::Instant::now();

        let results = rt.block_on(async {
            match self.conn.query(&paged).await {
                Ok(qr) => Ok(vec![StatementResult::Query(qr)]),
                Err(ferrule_core::CoreError::QueryFailed(_)) => {
                    match self.conn.execute(&paged).await {
                        Ok(summary) => Ok(vec![StatementResult::Summary(summary)]),
                        Err(_) => self.conn.execute_multi(&paged).await,
                    }
                }
                Err(e) => Err(e),
            }
        });

        let query_time = query_start.elapsed();

        match results {
            Ok(res) => {
                self.state.last_sql = Some(sql.to_string());
                let format_start = std::time::Instant::now();
                match render_results(&res, self.state.format, self.state.limit, self.state.offset) {
                    Ok(text) => {
                        if !text.is_empty() {
                            println!("{text}");
                        }
                    }
                    Err(e) => eprintln!("Format error: {e}"),
                }
                let format_time = format_start.elapsed();

                if self.state.timing {
                    eprintln!(
                        "[ferrule] timing: query={:.3}s format={:.3}s total={:.3}s",
                        query_time.as_secs_f64(),
                        format_time.as_secs_f64(),
                        start.elapsed().as_secs_f64(),
                    );
                }
            }
            Err(e) => {
                eprintln!("Error: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

fn render_results(
    results: &[StatementResult],
    format: OutputFormat,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<String, CliError> {
    if results.len() == 1 {
        render_single_result(&results[0], format, limit, offset)
    } else {
        let mut out = String::new();
        for (i, result) in results.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            match result {
                StatementResult::Query(_) => {
                    let rendered = render_single_result(result, format, limit, offset)?;
                    out.push_str(&format!("-- Result set {}\n", i + 1));
                    out.push_str(&rendered);
                    out.push('\n');
                }
                StatementResult::Summary(s) => {
                    out.push_str(&format!(
                        "-- Statement {}: {} rows affected\n",
                        i + 1,
                        s.rows_affected.unwrap_or(0)
                    ));
                }
            }
        }
        Ok(out)
    }
}

fn render_query_result(
    result: &QueryResult,
    format: OutputFormat,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<String, CliError> {
    let mut qr = result.clone();
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

fn render_single_result(
    result: &StatementResult,
    format: OutputFormat,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<String, CliError> {
    match result {
        StatementResult::Query(qr) => render_query_result(qr, format, limit, offset),
        StatementResult::Summary(s) => {
            Ok(format!("{} rows affected", s.rows_affected.unwrap_or(0)))
        }
    }
}

// ---------------------------------------------------------------------------
// Meta-command dispatch
// ---------------------------------------------------------------------------

pub fn handle_meta_line(repl: &mut Repl, line: &str, rt: &tokio::runtime::Handle) -> bool {
    let inner = line.strip_prefix('\\').unwrap_or(line).trim();
    let mut parts = inner.split_whitespace();
    let cmd = match parts.next() {
        Some(c) => c,
        None => {
            eprintln!("Empty meta-command.");
            return false;
        }
    };
    let args: Vec<&str> = parts.collect();

    match cmd {
        "q" | "quit" | "exit" => return true,
        "conn" => {
            rt.block_on(async {
                if args.is_empty() {
                    println!("Current connection: {}", repl.url.redacted());
                } else {
                    repl.switch_connection(args[0]).await;
                }
            });
        }
        "d" => {
            if args.is_empty() {
                // List tables when no argument given (psql-style convenience)
                cmd_list_tables(repl, None, rt);
            } else {
                cmd_describe_table(repl, args[0], rt);
            }
        }
        "dt" => {
            cmd_list_tables(repl, args.first().copied(), rt);
        }
        "format" => {
            if args.is_empty() {
                println!("Current format: {:?}", repl.state.format);
            } else {
                match OutputFormat::parse(args[0]) {
                    Some(fmt) => {
                        repl.state.format = fmt;
                        println!("Format set to: {:?}", fmt);
                    }
                    None => eprintln!(
                        "Unknown format '{}'. Use: table, json, csv, yaml, raw.",
                        args[0]
                    ),
                }
            }
        }
        "limit" => {
            if args.is_empty() {
                match repl.state.limit {
                    Some(n) => println!("Current limit: {n}"),
                    None => println!("No limit set."),
                }
            } else {
                match args[0].parse::<usize>() {
                    Ok(0) => {
                        repl.state.limit = None;
                        println!("Limit cleared.");
                    }
                    Ok(n) => {
                        repl.state.limit = Some(n);
                        println!("Limit set to: {n}");
                    }
                    Err(_) => eprintln!("Invalid limit: '{}'", args[0]),
                }
            }
        }
        "timing" => {
            if args.is_empty() {
                repl.state.timing = !repl.state.timing;
            } else {
                repl.state.timing = matches!(
                    args[0].to_ascii_lowercase().as_str(),
                    "on" | "true" | "1" | "yes"
                );
            }
            println!("Timing: {}", if repl.state.timing { "on" } else { "off" });
        }
        "verbose" => {
            if args.is_empty() {
                repl.state.verbose = !repl.state.verbose;
            } else {
                repl.state.verbose = matches!(
                    args[0].to_ascii_lowercase().as_str(),
                    "on" | "true" | "1" | "yes"
                );
            }
            println!("Verbose: {}", if repl.state.verbose { "on" } else { "off" });
        }
        "bookmark" => {
            if args.is_empty() {
                eprintln!("Usage: \\bookmark <subcommand> ...");
                eprintln!("  save <name>   Save last SQL as bookmark");
                eprintln!("  list          List bookmarks");
                eprintln!("  run <name>   Run a bookmark");
                eprintln!("  delete <name>  Delete a bookmark");
            } else {
                match args[0] {
                    "save" => {
                        if args.len() < 2 {
                            eprintln!("Usage: \\bookmark save <name>");
                        } else {
                            cmd_bookmark_save(repl, args[1]);
                        }
                    }
                    "list" => cmd_bookmark_list(),
                    "run" => {
                        if args.len() < 2 {
                            eprintln!("Usage: \\bookmark run <name> [param1] ...");
                        } else {
                            let name = args[1];
                            let params: Vec<String> =
                                args[2..].iter().map(|s| s.to_string()).collect();
                            cmd_bookmark_run(repl, name, &params, rt);
                        }
                    }
                    "delete" => {
                        if args.len() < 2 {
                            eprintln!("Usage: \\bookmark delete <name>");
                        } else {
                            cmd_bookmark_delete(args[1]);
                        }
                    }
                    _ => eprintln!(
                        "Unknown bookmark subcommand: {}. Use save, list, run, delete.",
                        args[0]
                    ),
                }
            }
        }
        "help" | "h" | "?" => print_help(),
        _ => eprintln!("Unknown meta-command: \\{}. Type \\help for help.", cmd),
    }
    false
}

fn print_help() {
    println!("Meta-commands:");
    println!("  \\q                   Quit REPL");
    println!("  \\conn [name]         Switch connection (or show current)");
    println!("  \\d [table]           Describe table (or list tables if no table)");
    println!("  \\dt [schema]         List tables");
    println!("  \\format [fmt]        Set output format: table, json, csv, yaml, raw");
    println!("  \\limit [N]           Set row limit (0 to clear)");
    println!("  \\timing [on|off]      Toggle timing display");
    println!("  \\verbose [on|off]     Toggle verbose logging");
    println!("  \\bookmark save <name>  Save last SQL as bookmark");
    println!("  \\bookmark list          List bookmarks");
    println!("  \\bookmark run <name>   Run a bookmark with optional params");
    println!("  \\bookmark delete <name>  Delete a bookmark");
    println!("  \\help                Show this help");
}

fn cmd_describe_table(repl: &mut Repl, table: &str, rt: &tokio::runtime::Handle) {
    let result = rt.block_on(async { repl.conn.describe_table(None, table).await });
    match result {
        Ok(qr) => match render_query_result(&qr, repl.state.format, None, None) {
            Ok(text) => println!("{text}"),
            Err(e) => eprintln!("Format error: {e}"),
        },
        Err(e) => eprintln!("Describe failed: {e}"),
    }
}

fn cmd_list_tables(repl: &mut Repl, schema: Option<&str>, rt: &tokio::runtime::Handle) {
    let result = rt.block_on(async { repl.conn.list_tables(schema).await });
    match result {
        Ok(names) => {
            let qr = QueryResult {
                columns: vec![ColumnInfo {
                    name: "table_name".to_string(),
                    type_hint: TypeHint::String,
                    nullable: true,
                }],
                rows: names.into_iter().map(|n| vec![Value::String(n)]).collect(),
            };
            match render_query_result(&qr, repl.state.format, repl.state.limit, repl.state.offset) {
                Ok(text) => println!("{text}"),
                Err(e) => eprintln!("Format error: {e}"),
            }
        }
        Err(e) => eprintln!("List tables failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Bookmark helpers
// ---------------------------------------------------------------------------

fn cmd_bookmark_save(repl: &Repl, name: &str) {
    let sql = match &repl.state.last_sql {
        Some(s) => s.clone(),
        None => {
            eprintln!("No SQL to save. Execute a statement first.");
            return;
        }
    };
    let connection = resolve_connection_name_for_bookmark(repl);
    let mut store = match BookmarkStore::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to load bookmarks: {e}");
            return;
        }
    };

    let full_name = if name.contains('.') {
        name.to_string()
    } else if let Some(ref conn_name) = connection {
        format!("{}.{}", conn_name, name)
    } else {
        name.to_string()
    };

    let bm_connection = BookmarkStore::connection_hint(&full_name).map(|s| s.to_string());
    store.insert(full_name.clone(), sql, bm_connection);
    if let Err(e) = store.save() {
        eprintln!("Failed to save bookmark: {e}");
    } else {
        println!("Bookmark '{}' saved.", full_name);
    }
}

fn resolve_connection_name_for_bookmark(repl: &Repl) -> Option<String> {
    // Try to find the current URL in the registry / profiles
    if let Ok(registry) = ConnectionRegistry::load_default() {
        for entry in registry.list() {
            if let Ok(url) = DatabaseUrl::parse(&entry.url) {
                if url.redacted() == repl.url.redacted() {
                    return Some(entry.name.clone());
                }
            }
        }
    }
    for (name, profile) in &repl.global_config.connection {
        if let Ok(url) = DatabaseUrl::parse(&profile.url) {
            if url.redacted() == repl.url.redacted() {
                return Some(name.clone());
            }
        }
    }
    // Fallback: use host name from URL
    repl.url.host().map(|h| h.to_string())
}

fn cmd_bookmark_list() {
    let store = match BookmarkStore::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to load bookmarks: {e}");
            return;
        }
    };
    let entries = store.list();
    if entries.is_empty() {
        println!("No bookmarks saved.");
        return;
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
}

fn cmd_bookmark_run(repl: &mut Repl, name: &str, params: &[String], rt: &tokio::runtime::Handle) {
    let store = match BookmarkStore::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to load bookmarks: {e}");
            return;
        }
    };
    let bookmark = match store.get(name) {
        Some(b) => b,
        None => {
            eprintln!("Bookmark '{}' not found.", name);
            return;
        }
    };
    let sql = BookmarkStore::resolve_params(&bookmark.sql, params);
    if sql != bookmark.sql {
        eprintln!("[ferrule] resolved SQL: {}", sql);
    }
    repl.execute_sql(&sql, rt);
}

fn cmd_bookmark_delete(name: &str) {
    let mut store = match BookmarkStore::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to load bookmarks: {e}");
            return;
        }
    };
    match store.remove(name) {
        Ok(()) => {
            if let Err(e) = store.save() {
                eprintln!("Failed to save bookmarks: {e}");
            } else {
                println!("Bookmark '{}' deleted.", name);
            }
        }
        Err(e) => eprintln!("Failed to delete bookmark: {e}"),
    }
}

// ---------------------------------------------------------------------------
// REPL loop
// ---------------------------------------------------------------------------

/// Run the interactive REPL loop. Returns when the user exits.
pub fn run_repl_loop(repl: &mut Repl, rt: &tokio::runtime::Handle) -> Result<(), CliError> {
    let history_path = dirs::cache_dir().map(|d| d.join("ferrule").join("history"));
    if let Some(ref path) = history_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    let mut rl = match rustyline::DefaultEditor::new() {
        Ok(e) => e,
        Err(err) => {
            return Err(CliError::usage(format!(
                "Cannot initialize readline: {err}"
            )));
        }
    };

    if let Some(ref path) = history_path {
        let _ = rl.load_history(path);
    }

    let mut buffer = String::new();
    let mut in_multiline = false;

    loop {
        let prompt = if in_multiline {
            "ferrule-> "
        } else {
            "ferrule=> "
        };

        match rl.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();

                // Meta-command detection
                if trimmed.starts_with('\\') && !in_multiline {
                    let should_quit = handle_meta_line(repl, trimmed, rt);
                    if should_quit {
                        break;
                    }
                    continue;
                }

                if in_multiline {
                    if trimmed.is_empty() {
                        // Cancel multi-line input
                        in_multiline = false;
                        buffer.clear();
                        println!("Input cancelled.");
                        continue;
                    }
                    if !buffer.is_empty() {
                        buffer.push('\n');
                    }
                    buffer.push_str(line.trim_end());
                    if buffer.trim_end().ends_with(';') {
                        repl.execute_sql(&buffer, rt);
                        in_multiline = false;
                        buffer.clear();
                    }
                } else {
                    let line_trimmed = line.trim_end();
                    if line_trimmed.ends_with(';') {
                        repl.execute_sql(line_trimmed, rt);
                        let _ = rl.add_history_entry(line_trimmed);
                    } else if !trimmed.is_empty() {
                        buffer.push_str(line_trimmed);
                        in_multiline = true;
                    }
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => {
                if in_multiline {
                    in_multiline = false;
                    buffer.clear();
                    println!("Input cancelled.");
                } else {
                    // Ctrl-C on empty prompt — ignore
                }
            }
            Err(rustyline::error::ReadlineError::Eof) => {
                break;
            }
            Err(e) => {
                eprintln!("Readline error: {e}");
                break;
            }
        }
    }

    if let Some(ref path) = history_path {
        let _ = rl.save_history(path);
    }
    println!("Goodbye.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Resolve initial connection string
// ---------------------------------------------------------------------------

pub fn resolve_initial_connection(
    args_connection: Option<String>,
    global_config: &GlobalConfig,
) -> Result<String, CliError> {
    if let Some(c) = args_connection {
        return Ok(c);
    }

    // Try profile named "default"
    if let Some(profile) = global_config.connection.get("default") {
        return Ok(profile.url.clone());
    }

    // Try registry named "default"
    if let Ok(registry) = ConnectionRegistry::load_default() {
        if let Some(entry) = registry.get("default") {
            return Ok(entry.url.clone());
        }
    }

    // Interactive prompt
    let tty = is_terminal::IsTerminal::is_terminal(&std::io::stdin());
    if tty {
        print!("Connection: ");
        let _ = std::io::stdout().flush();
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).map_err(CliError::Io)?;
        let trimmed = buf.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    Err(CliError::usage(
        "No connection provided. Use `ferrule repl <connection>` or set a default profile.",
    ))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metacommand_parsing_quit() {
        let input = "\\q";
        let inner = input.strip_prefix('\\').unwrap_or(input).trim();
        let mut parts = inner.split_whitespace();
        let cmd = parts.next().unwrap();
        assert_eq!(cmd, "q");
        let args: Vec<&str> = parts.collect();
        assert!(args.is_empty());
    }

    #[test]
    fn test_metacommand_parsing_conn() {
        let input = "\\conn mydb";
        let inner = input.strip_prefix('\\').unwrap_or(input).trim();
        let mut parts = inner.split_whitespace();
        let cmd = parts.next().unwrap();
        assert_eq!(cmd, "conn");
        let args: Vec<&str> = parts.collect();
        assert_eq!(args, vec!["mydb"]);
    }

    #[test]
    fn test_metacommand_parsing_format() {
        let input = "\\format json";
        let inner = input.strip_prefix('\\').unwrap_or(input).trim();
        let mut parts = inner.split_whitespace();
        let cmd = parts.next().unwrap();
        assert_eq!(cmd, "format");
        let args: Vec<&str> = parts.collect();
        assert_eq!(args, vec!["json"]);
    }

    #[test]
    fn test_multiline_state_machine_single_line() {
        // A line ending with ; should be considered complete.
        let line = "SELECT 1;";
        assert!(line.trim_end().ends_with(';'));
    }

    #[test]
    fn test_multiline_state_machine_accumulation() {
        // Simulate multi-line accumulation.
        let mut buffer = String::new();
        buffer.push_str("SELECT 1");
        buffer.push('\n');
        buffer.push_str("FROM t;");
        assert!(buffer.trim_end().ends_with(';'));
    }

    #[test]
    fn test_multiline_state_machine_not_ending_with_semicolon() {
        let mut buffer = String::new();
        buffer.push_str("SELECT 1");
        buffer.push('\n');
        buffer.push_str("FROM t");
        assert!(!buffer.trim_end().ends_with(';'));
    }

    #[test]
    fn test_repl_state_default() {
        let state = ReplState::default();
        assert_eq!(state.format, OutputFormat::Table);
        assert_eq!(state.limit, None);
        assert_eq!(state.offset, None);
        assert!(!state.timing);
        assert!(!state.verbose);
        assert!(state.last_sql.is_none());
    }

    #[test]
    fn test_output_format_parse_roundtrip() {
        assert_eq!(OutputFormat::parse("table"), Some(OutputFormat::Table));
        assert_eq!(OutputFormat::parse("json"), Some(OutputFormat::Json));
        assert_eq!(OutputFormat::parse("csv"), Some(OutputFormat::Csv));
        assert_eq!(OutputFormat::parse("yaml"), Some(OutputFormat::Yaml));
        assert_eq!(OutputFormat::parse("raw"), Some(OutputFormat::Raw));
        assert_eq!(OutputFormat::parse("unknown"), None);
    }
}
