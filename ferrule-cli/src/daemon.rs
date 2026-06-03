use crate::error::CliError;
use dashmap::DashMap;
use ferrule_core::formatter::{format_result, OutputFormat};
use ferrule_sql::connection::{ConnectOptions, StatementResult};
use ferrule_sql::url::DatabaseUrl;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

// ---------------------------------------------------------------------------
// Protocol types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Stop,
    Query {
        sql: String,
        url: String,
        insecure: bool,
        format: String,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    Execute {
        sql: String,
        url: String,
        insecure: bool,
    },
    ListTables {
        url: String,
        insecure: bool,
        schema: Option<String>,
    },
    DescribeTable {
        url: String,
        insecure: bool,
        schema: Option<String>,
        table: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok { payload: String },
    Err { message: String },
}

// ---------------------------------------------------------------------------
// Pooling
// ---------------------------------------------------------------------------

struct PooledConnection {
    conn: Mutex<Box<dyn ferrule_sql::Connection>>,
    last_used: std::time::Instant,
}

type Pool = Arc<DashMap<String, PooledConnection>>;

fn get_or_connect(pool: &Pool, url: &DatabaseUrl, insecure: bool) -> Result<(), String> {
    let key = url.raw().to_string();

    if pool.contains_key(&key) {
        if let Some(mut entry) = pool.get_mut(&key) {
            entry.last_used = std::time::Instant::now();
        }
        return Ok(());
    }

    let opts = ConnectOptions {
        insecure,
        password: None,
    };
    let conn = ferrule_sql::connect(url, &opts, None).map_err(|e| e.to_string())?;

    pool.insert(
        key,
        PooledConnection {
            conn: Mutex::new(conn),
            last_used: std::time::Instant::now(),
        },
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

fn handle_request(req: Request, pool: &Pool, stop_flag: &AtomicBool) -> Response {
    match req {
        Request::Ping => Response::Ok {
            payload: "pong".into(),
        },
        Request::Stop => {
            stop_flag.store(true, Ordering::Relaxed);
            Response::Ok {
                payload: "stopping".into(),
            }
        }
        Request::Query {
            sql,
            url,
            insecure,
            format,
            limit,
            offset,
        } => {
            let db_url = match DatabaseUrl::parse(&url) {
                Ok(u) => u,
                Err(e) => {
                    return Response::Err {
                        message: format!("Invalid URL: {e}"),
                    };
                }
            };

            let backend = match ferrule_sql::Backend::from_scheme(db_url.scheme()) {
                Some(b) => b,
                None => {
                    return Response::Err {
                        message: format!("Unsupported scheme: {}", db_url.scheme()),
                    };
                }
            };

            let paged_sql = match ferrule_sql::apply_paging(&sql, limit, offset, backend) {
                Ok(s) => s,
                Err(e) => {
                    return Response::Err {
                        message: e.to_string(),
                    };
                }
            };

            let fmt = OutputFormat::parse(&format).unwrap_or(OutputFormat::Json);

            if let Err(e) = get_or_connect(pool, &db_url, insecure) {
                return Response::Err { message: e };
            }

            let entry = pool.get(db_url.raw()).expect("pool entry");
            let mut guard = entry.conn.lock().unwrap();

            let results = match guard.query(&paged_sql) {
                Ok(qr) => vec![StatementResult::Query(qr)],
                Err(ferrule_sql::SqlError::QueryFailed(_)) => match guard.execute(&paged_sql) {
                    Ok(summary) => {
                        vec![StatementResult::Summary(summary)]
                    }
                    Err(_) => match guard.execute_multi(&paged_sql) {
                        Ok(res) => res,
                        Err(e) => {
                            return Response::Err {
                                message: e.to_string(),
                            };
                        }
                    },
                },
                Err(e) => {
                    return Response::Err {
                        message: e.to_string(),
                    };
                }
            };

            drop(guard);
            drop(entry);

            let rendered = match results.as_slice() {
                [single] => render_statement(single, fmt, limit, offset),
                _ => {
                    let mut out = String::new();
                    for (i, r) in results.iter().enumerate() {
                        if i > 0 {
                            out.push('\n');
                        }
                        out.push_str(&format!("-- Result set {}\n", i + 1));
                        out.push_str(&render_statement(r, fmt, limit, offset));
                    }
                    out
                }
            };

            Response::Ok { payload: rendered }
        }
        Request::Execute { sql, url, insecure } => {
            let db_url = match DatabaseUrl::parse(&url) {
                Ok(u) => u,
                Err(e) => {
                    return Response::Err {
                        message: format!("Invalid URL: {e}"),
                    };
                }
            };

            if let Err(e) = get_or_connect(pool, &db_url, insecure) {
                return Response::Err { message: e };
            }

            let entry = pool.get(db_url.raw()).expect("pool entry");
            let mut guard = entry.conn.lock().unwrap();

            let summary = match guard.execute(&sql) {
                Ok(s) => s,
                Err(e) => {
                    return Response::Err {
                        message: e.to_string(),
                    };
                }
            };

            drop(guard);
            drop(entry);

            Response::Ok {
                payload: format!("{} rows affected", summary.rows_affected.unwrap_or(0)),
            }
        }
        Request::ListTables {
            url,
            insecure,
            schema,
        } => {
            let db_url = match DatabaseUrl::parse(&url) {
                Ok(u) => u,
                Err(e) => {
                    return Response::Err {
                        message: format!("Invalid URL: {e}"),
                    };
                }
            };

            if let Err(e) = get_or_connect(pool, &db_url, insecure) {
                return Response::Err { message: e };
            }

            let entry = pool.get(db_url.raw()).expect("pool entry");
            let mut guard = entry.conn.lock().unwrap();

            let names = match guard.list_tables(schema.as_deref()) {
                Ok(n) => n,
                Err(e) => {
                    return Response::Err {
                        message: e.to_string(),
                    };
                }
            };

            drop(guard);
            drop(entry);

            Response::Ok {
                payload: names.join("\n"),
            }
        }
        Request::DescribeTable {
            url,
            insecure,
            schema,
            table,
        } => {
            let db_url = match DatabaseUrl::parse(&url) {
                Ok(u) => u,
                Err(e) => {
                    return Response::Err {
                        message: format!("Invalid URL: {e}"),
                    };
                }
            };

            if let Err(e) = get_or_connect(pool, &db_url, insecure) {
                return Response::Err { message: e };
            }

            let entry = pool.get(db_url.raw()).expect("pool entry");
            let mut guard = entry.conn.lock().unwrap();

            let result = match guard.describe_table(schema.as_deref(), &table) {
                Ok(r) => r,
                Err(e) => {
                    return Response::Err {
                        message: e.to_string(),
                    };
                }
            };

            drop(guard);
            drop(entry);

            let fmt = OutputFormat::Table;
            match format_result(&result, fmt) {
                Ok(s) => Response::Ok { payload: s },
                Err(e) => Response::Err {
                    message: e.to_string(),
                },
            }
        }
    }
}

fn render_statement(
    result: &StatementResult,
    format: OutputFormat,
    limit: Option<usize>,
    offset: Option<usize>,
) -> String {
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
            format_result(&qr, format).unwrap_or_else(|e| e.to_string())
        }
        StatementResult::Summary(s) => {
            format!("{} rows affected", s.rows_affected.unwrap_or(0))
        }
    }
}

// ---------------------------------------------------------------------------
// Transport helpers
// ---------------------------------------------------------------------------

fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("ferrule"))
}

fn pid_file() -> Option<PathBuf> {
    cache_dir().map(|d| d.join("daemon.pid"))
}

#[cfg(unix)]
fn socket_path() -> Option<PathBuf> {
    cache_dir().map(|d| d.join("daemon.sock"))
}

#[cfg(not(unix))]
fn port_file() -> Option<PathBuf> {
    cache_dir().map(|d| d.join("daemon.port"))
}

#[cfg(unix)]
async fn bind_listener() -> Result<tokio::net::UnixListener, CliError> {
    let path = socket_path().ok_or_else(|| CliError::usage("Could not determine socket path."))?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(CliError::Io)?;
    }
    let _ = tokio::fs::remove_file(&path).await;
    let listener = tokio::net::UnixListener::bind(&path).map_err(CliError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, perms).map_err(CliError::Io)?;
    }
    Ok(listener)
}

#[cfg(not(unix))]
async fn bind_listener() -> Result<tokio::net::TcpListener, CliError> {
    let dir = cache_dir().ok_or_else(|| CliError::usage("Could not determine cache directory."))?;
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(CliError::Io)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(CliError::Io)?;
    let port = listener.local_addr().map_err(CliError::Io)?.port();
    let port_path = port_file().unwrap();
    tokio::fs::write(&port_path, port.to_string())
        .await
        .map_err(CliError::Io)?;
    Ok(listener)
}

async fn read_json_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Request, Box<dyn std::error::Error + Send + Sync>> {
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let req = serde_json::from_str(line.trim())?;
    Ok(req)
}

async fn write_json_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    resp: &Response,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let json = serde_json::to_string(resp)?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub async fn run_daemon_server() -> Result<(), CliError> {
    let pool: Pool = Arc::new(DashMap::new());
    let stop_flag = Arc::new(AtomicBool::new(false));

    #[cfg(unix)]
    let listener = bind_listener().await?;
    #[cfg(not(unix))]
    let listener = bind_listener().await?;

    // Write PID file
    if let Some(pid_path) = pid_file() {
        let pid = std::process::id().to_string();
        tokio::fs::write(&pid_path, pid).await.ok();
    }

    println!("[ferrule-daemon] listening");

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        let accept_fut = accept_conn(&listener);
        let timeout = tokio::time::Duration::from_secs(1);
        match tokio::time::timeout(timeout, accept_fut).await {
            Ok(Ok(stream)) => {
                let pool = pool.clone();
                let stop_flag = stop_flag.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, pool, stop_flag).await {
                        eprintln!("[ferrule-daemon] client error: {e}");
                    }
                });
            }
            Ok(Err(e)) => {
                eprintln!("[ferrule-daemon] accept error: {e}");
            }
            Err(_) => {
                // Timeout — loop back to check stop_flag
            }
        }
    }

    println!("[ferrule-daemon] shutting down gracefully");

    // Cleanup
    #[cfg(unix)]
    if let Some(path) = socket_path() {
        let _ = tokio::fs::remove_file(path).await;
    }
    #[cfg(not(unix))]
    if let Some(path) = port_file() {
        let _ = tokio::fs::remove_file(path).await;
    }
    if let Some(path) = pid_file() {
        let _ = tokio::fs::remove_file(path).await;
    }

    Ok(())
}

#[cfg(unix)]
async fn accept_conn(
    listener: &tokio::net::UnixListener,
) -> Result<tokio::net::UnixStream, std::io::Error> {
    let (stream, _) = listener.accept().await?;
    Ok(stream)
}

#[cfg(not(unix))]
async fn accept_conn(
    listener: &tokio::net::TcpListener,
) -> Result<tokio::net::TcpStream, std::io::Error> {
    let (stream, _) = listener.accept().await?;
    Ok(stream)
}

#[cfg(unix)]
async fn handle_client(
    stream: tokio::net::UnixStream,
    pool: Pool,
    stop_flag: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let req = read_json_line(&mut reader).await?;
    // `handle_request` is synchronous and blocks on the pooled
    // ferrule-sql connection (each connection owns a private runtime).
    // Run it on a blocking thread so that inner `block_on` never nests
    // inside the daemon's own runtime.
    let resp = tokio::task::spawn_blocking(move || handle_request(req, &pool, &stop_flag)).await?;
    write_json_response(&mut write, &resp).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn handle_client(
    stream: tokio::net::TcpStream,
    pool: Pool,
    stop_flag: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let req = read_json_line(&mut reader).await?;
    // See the unix arm: run the blocking, runtime-owning request
    // handler off the daemon runtime via spawn_blocking.
    let resp = tokio::task::spawn_blocking(move || handle_request(req, &pool, &stop_flag)).await?;
    write_json_response(&mut write, &resp).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Client helpers
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn send_request(req: &Request) -> Result<Response, CliError> {
    let path = socket_path().ok_or_else(|| CliError::usage("Daemon socket not found."))?;
    let mut stream = tokio::net::UnixStream::connect(&path)
        .await
        .map_err(|e| CliError::usage(format!("Cannot connect to daemon: {e}")))?;
    let json = serde_json::to_string(req).map_err(|e| CliError::usage(e.to_string()))?;
    tokio::io::AsyncWriteExt::write_all(&mut stream, json.as_bytes())
        .await
        .map_err(CliError::Io)?;
    tokio::io::AsyncWriteExt::write_all(&mut stream, b"\n")
        .await
        .map_err(CliError::Io)?;
    let mut buf = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut buf).await.map_err(CliError::Io)?;
    serde_json::from_str(&buf).map_err(|e| CliError::usage(format!("Invalid daemon response: {e}")))
}

#[cfg(not(unix))]
async fn send_request(req: &Request) -> Result<Response, CliError> {
    let port_path = port_file().ok_or_else(|| CliError::usage("Daemon port file not found."))?;
    let port_str = tokio::fs::read_to_string(&port_path)
        .await
        .map_err(|e| CliError::usage(format!("Cannot read daemon port: {e}")))?;
    let port: u16 = port_str
        .trim()
        .parse()
        .map_err(|e| CliError::usage(format!("Invalid daemon port: {e}")))?;
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| CliError::usage(format!("Cannot connect to daemon: {e}")))?;
    let json = serde_json::to_string(req).map_err(|e| CliError::usage(e.to_string()))?;
    tokio::io::AsyncWriteExt::write_all(&mut stream, json.as_bytes())
        .await
        .map_err(CliError::Io)?;
    tokio::io::AsyncWriteExt::write_all(&mut stream, b"\n")
        .await
        .map_err(CliError::Io)?;
    let mut buf = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut buf).await.map_err(CliError::Io)?;
    serde_json::from_str(&buf).map_err(|e| CliError::usage(format!("Invalid daemon response: {e}")))
}

// ---------------------------------------------------------------------------
// Sync client wrappers
//
// The daemon *client* does only socket / filesystem I/O — it never opens
// a ferrule-sql connection — so it is safe to drive on a short-lived
// local current-thread runtime. This keeps the daemon transport async
// (tokio UnixStream / TcpStream) while the command layer that calls it
// stays synchronous (#64).
// ---------------------------------------------------------------------------

/// Build a throwaway current-thread runtime for one client round-trip.
fn client_runtime() -> Result<tokio::runtime::Runtime, CliError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(CliError::Io)
}

/// Blocking wrapper over [`send_request`].
fn send_request_blocking(req: &Request) -> Result<Response, CliError> {
    client_runtime()?.block_on(send_request(req))
}

// ---------------------------------------------------------------------------
// Public API used by commands/conn.rs
// ---------------------------------------------------------------------------

pub fn start_daemon(background: bool) -> Result<(), CliError> {
    if is_daemon_running() {
        println!("Daemon is already running.");
        return Ok(());
    }

    if background {
        let exe = std::env::current_exe().map_err(CliError::Io)?;
        let child = std::process::Command::new(exe)
            .arg("__daemon")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(CliError::Io)?;
        println!("Daemon started (PID {}).", child.id());
    } else {
        println!("Starting daemon in foreground... Press Ctrl-C to stop.");
        // Foreground server runs its own async event loop on a dedicated
        // runtime; it creates pooled ferrule-sql connections only inside
        // spawn_blocking, so there is no nested-runtime hazard.
        client_runtime()?.block_on(run_daemon_server())?;
    }
    Ok(())
}

pub fn stop_daemon() -> Result<(), CliError> {
    // Try graceful shutdown via IPC first
    match send_request_blocking(&Request::Stop) {
        Ok(Response::Ok { .. }) => {
            println!("Daemon stopped.");
        }
        _ => {
            // Fallback: kill via PID file
            if let Some(pid_path) = pid_file() {
                if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
                    if let Ok(pid) = pid_str.trim().parse::<u32>() {
                        #[cfg(unix)]
                        {
                            let _ = std::process::Command::new("kill")
                                .arg("-TERM")
                                .arg(pid.to_string())
                                .output();
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = std::process::Command::new("taskkill")
                                .arg("/PID")
                                .arg(pid.to_string())
                                .arg("/F")
                                .output();
                        }
                    }
                }
            }
            // Clean up stale files
            #[cfg(unix)]
            if let Some(path) = socket_path() {
                let _ = std::fs::remove_file(path);
            }
            #[cfg(not(unix))]
            if let Some(path) = port_file() {
                let _ = std::fs::remove_file(path);
            }
            if let Some(path) = pid_file() {
                let _ = std::fs::remove_file(path);
            }
            println!("Daemon stopped (fallback).");
        }
    }
    Ok(())
}

pub fn daemon_status() -> Result<(), CliError> {
    match send_request_blocking(&Request::Ping) {
        Ok(Response::Ok { payload }) => {
            println!("Daemon is running. Response: {}", payload);
        }
        Ok(Response::Err { message }) => {
            println!("Daemon responded with error: {}", message);
        }
        Err(e) => {
            println!("Daemon is not running: {}", e);
        }
    }
    Ok(())
}

pub fn is_daemon_running() -> bool {
    matches!(
        send_request_blocking(&Request::Ping),
        Ok(Response::Ok { .. })
    )
}

/// Execute a query through the daemon, returning the formatted payload.
pub fn daemon_query(
    sql: &str,
    url: &DatabaseUrl,
    insecure: bool,
    format: OutputFormat,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<String, CliError> {
    let req = Request::Query {
        sql: sql.into(),
        url: url.raw().into(),
        insecure,
        format: format_to_string(format),
        limit,
        offset,
    };
    match send_request_blocking(&req)? {
        Response::Ok { payload } => Ok(payload),
        Response::Err { message } => Err(CliError::usage(message)),
    }
}

pub fn daemon_tables(
    url: &DatabaseUrl,
    insecure: bool,
    schema: Option<&str>,
) -> Result<String, CliError> {
    let req = Request::ListTables {
        url: url.raw().into(),
        insecure,
        schema: schema.map(|s| s.to_string()),
    };
    match send_request_blocking(&req)? {
        Response::Ok { payload } => Ok(payload),
        Response::Err { message } => Err(CliError::usage(message)),
    }
}

pub fn daemon_describe(
    url: &DatabaseUrl,
    insecure: bool,
    schema: Option<&str>,
    table: &str,
) -> Result<String, CliError> {
    let req = Request::DescribeTable {
        url: url.raw().into(),
        insecure,
        schema: schema.map(|s| s.to_string()),
        table: table.into(),
    };
    match send_request_blocking(&req)? {
        Response::Ok { payload } => Ok(payload),
        Response::Err { message } => Err(CliError::usage(message)),
    }
}

fn format_to_string(format: OutputFormat) -> String {
    match format {
        OutputFormat::Table => "table".into(),
        OutputFormat::Json => "json".into(),
        OutputFormat::Csv => "csv".into(),
        OutputFormat::Yaml => "yaml".into(),
        OutputFormat::Raw => "raw".into(),
        OutputFormat::Markdown => "markdown".into(),
        OutputFormat::Jsonl => "jsonl".into(),
        OutputFormat::Html => "html".into(),
    }
}
