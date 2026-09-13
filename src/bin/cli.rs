//! Interactive CLI shell for rustqlite.
//!
//! Usage:
//!   rustqlite-cli [OPTIONS] [DB_PATH]
//!
//! Database options:
//!   --sqlite-format     Create the database file in SQLite's own disk
//!                       format (fileformat2) — openable by the `sqlite3`
//!                       CLI and every SQLite driver. Existing SQLite files
//!                       are detected automatically regardless of the flag.
//!
//! Remote mode (SCRAM-SHA-256 authenticated server):
//!   --connect URL       Connect to a rustqlite-server instead of opening
//!                       a file (e.g. http://127.0.0.1:8080).
//!   --user NAME         Username for --connect.
//!   --password PW       Password (otherwise: prompt / $RUSTQLITE_PASSWORD).
//!
//! Batch operations (run once, then exit — scriptable):
//!   --dump FILE         Full SQL dump (schema + data) to FILE ("-"=stdout).
//!   --export-schema FILE  CREATE statements only.
//!   --export-data FILE  INSERT statements only.
//!   --backup FILE       Physical byte-copy backup of the database.
//!   --import FILE       Execute an SQL script (schema and/or data).
//!   --import-csv FILE TABLE  Load CSV rows into TABLE (creates the table
//!                       from the header row when it does not exist).
//!
//! If DB_PATH is not given, opens an in-memory database.
//! Reads SQL statements from stdin (one per line, terminated by `;`).
//!
//! Special commands:
//!   .help              Show help.
//!   .tables            List all tables and views.
//!   .schema [pattern]  Show CREATE statements (optionally filtered).
//!   .dump [FILE]       Full SQL dump to FILE (default: stdout).
//!   .export-schema [FILE]  Schema only.
//!   .export-data [FILE]    Data only.
//!   .read FILE         Execute an SQL script.
//!   .import FILE [TABLE]   Import an SQL script, or CSV into TABLE.
//!   .backup FILE       Physical backup (byte copy of the db image).
//!   .mode json|table|csv|line  Output mode.
//!   .quit / .exit      Exit the shell.

use rustqlite::{Database, Value};
use std::env;
#[cfg(feature = "auth")]
use std::io::Read;
use std::io::{self, BufRead, Write};

#[cfg(feature = "auth")]
use rustqlite::auth::ClientExchange;
#[cfg(feature = "auth")]
use rustqlite::json::{decode_hex, encode_hex, Json};

/// Stdout wrapper that treats EPIPE as success: a reader leaving early
/// (`cli ... | grep -q ...`) must make the process exit cleanly instead
/// of panicking on the write unwrap — `set -o pipefail` pipelines depend
/// on it (SQLite's own CLI exits quietly on SIGPIPE).
struct PipeTolerant<W: std::io::Write>(W);

impl<W: std::io::Write> std::io::Write for PipeTolerant<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.write(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(buf.len()),
            Err(e) => Err(e),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.flush() {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// One batch operation requested from the command line.
enum BatchOp {
    Dump(Option<String>),
    ExportSchema(Option<String>),
    ExportData(Option<String>),
    Backup(String),
    Import(String),
    ImportCsv(String, String),
}

fn print_cli_help() {
    println!(
        "rustqlite-cli v{} — SQL shell for rustqlite",
        rustqlite::VERSION
    );
    println!();
    println!("Usage: rustqlite-cli [OPTIONS] [DB_PATH]");
    println!("  (no DB_PATH: in-memory database)");
    println!();
    println!("Database:");
    println!("  --sqlite-format         Create the file in SQLite's disk format");
    println!();
    println!("Remote mode:");
    println!("  --connect URL           Attach to a rustqlite-server");
    println!("  --user NAME             Username (with --connect)");
    println!("  --password PW           Password (otherwise prompt/env)");
    println!();
    println!("Batch operations (run, then exit):");
    println!("  --dump FILE             Full SQL dump ('-' = stdout)");
    println!("  --export-schema FILE    CREATE statements only");
    println!("  --export-data FILE      INSERT statements only");
    println!("  --backup FILE           Physical byte-copy backup");
    println!("  --import FILE           Execute an SQL script");
    println!("  --import-csv FILE TABLE Load CSV into TABLE");
    println!();
    println!("In the shell, .help lists dot commands.");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut sqlite_format = false;
    let mut path: Option<String> = None;
    let mut connect: Option<String> = None;
    let mut user: Option<String> = None;
    let mut password_flag: Option<String> = None;
    let mut batch: Option<BatchOp> = None;

    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        let value = |i: &mut usize| -> Option<String> {
            *i += 1;
            args.get(*i).cloned()
        };
        match arg {
            "--sqlite-format" => sqlite_format = true,
            "--connect" | "--server" => {
                if let Some(v) = value(&mut i) {
                    connect = Some(v);
                }
            }
            "--user" | "-u" => {
                if let Some(v) = value(&mut i) {
                    user = Some(v);
                }
            }
            "--password" => {
                if let Some(v) = value(&mut i) {
                    password_flag = Some(v);
                }
            }
            "--dump" => batch = Some(BatchOp::Dump(value(&mut i))),
            "--export-schema" => batch = Some(BatchOp::ExportSchema(value(&mut i))),
            "--export-data" => batch = Some(BatchOp::ExportData(value(&mut i))),
            "--backup" => match value(&mut i) {
                Some(v) => batch = Some(BatchOp::Backup(v)),
                None => {
                    eprintln!("error: --backup needs a FILE argument");
                    std::process::exit(1);
                }
            },
            "--import" => match value(&mut i) {
                Some(v) => batch = Some(BatchOp::Import(v)),
                None => {
                    eprintln!("error: --import needs a FILE argument");
                    std::process::exit(1);
                }
            },
            "--import-csv" => {
                let file = value(&mut i);
                let table = value(&mut i);
                match (file, table) {
                    (Some(f), Some(t)) => batch = Some(BatchOp::ImportCsv(f, t)),
                    _ => {
                        eprintln!("error: --import-csv needs FILE and TABLE arguments");
                        std::process::exit(1);
                    }
                }
            }
            "--help" | "-h" => {
                print_cli_help();
                return;
            }
            _ if arg.starts_with("--") => {
                eprintln!("unknown option: {}", arg);
                eprintln!("try --help");
                std::process::exit(1);
            }
            _ => {
                if path.is_some() {
                    eprintln!("error: multiple database paths given");
                    std::process::exit(1);
                }
                path = Some(arg.to_string());
            }
        }
        i += 1;
    }

    // ---- Build the runner: local database or remote session -----------
    #[cfg(feature = "auth")]
    let runner: Result<Runner, String> = if let Some(url) = connect {
        let user = user.unwrap_or_else(|| {
            eprintln!("error: --connect needs --user NAME");
            std::process::exit(1);
        });
        Runner::connect_remote(&url, &user, password_flag.as_deref())
    } else {
        open_local(path.as_deref(), sqlite_format)
    };
    #[cfg(not(feature = "auth"))]
    let runner: Result<Runner, String> = if connect.is_some() {
        Err(
            "this build lacks the `auth` feature; rebuild with --features auth for --connect"
                .to_string(),
        )
    } else {
        open_local(path.as_deref(), sqlite_format)
    };
    #[cfg(not(feature = "auth"))]
    {
        let _ = (connect, user, password_flag);
    }
    let runner = runner.unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(1);
    });

    let desc = runner.describe();
    println!("rustqlite v{} ({})", rustqlite::VERSION, desc);
    println!("Type .help for help, .quit to exit.");

    let stdout = io::stdout();
    let mut stdout = PipeTolerant(stdout.lock());

    // ---- Batch operations (scriptable one-shots) -----------------------
    let mut runner = runner;
    if let Some(op) = batch {
        let code = run_batch_op(&op, &mut runner, &mut stdout);
        let _ = stdout.flush();
        std::process::exit(code);
    }

    // ---- Interactive loop ----------------------------------------------
    let stdin = io::stdin();
    let mut runner = runner;
    let mut buffer = String::new();
    let mut mode = OutputMode::Table;

    loop {
        write!(
            stdout,
            "{}> ",
            if buffer.is_empty() {
                if runner.is_remote() {
                    "rustqlite-remote"
                } else {
                    "rustqlite"
                }
            } else {
                "  ..."
            }
        )
        .unwrap();
        stdout.flush().unwrap();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break; // EOF
        }
        let line = line.trim();
        if buffer.is_empty() && line.starts_with('.') {
            if let Err(e) = handle_dot_command(&mut runner, line, &mut mode, &mut stdout) {
                eprintln!("error: {}", e);
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        buffer.push_str(line);
        buffer.push('\n');
        if buffer.trim_end().ends_with(';') {
            let sql = buffer.trim().to_string();
            buffer.clear();
            match execute_sql(&mut runner, &sql, &mode, &mut stdout) {
                Ok(_) => {}
                Err(e) => eprintln!("error: {}", e),
            }
        }
    }
    println!();
}

// ---------------------------------------------------------------------------
// Runner: local database or authenticated remote session
// ---------------------------------------------------------------------------

enum Runner {
    Local(Box<Database>),
    #[cfg(feature = "auth")]
    Remote {
        url: String,
        token: String,
    },
}

impl Runner {
    fn describe(&self) -> String {
        match self {
            Runner::Local(db) => db.path().display().to_string(),
            #[cfg(feature = "auth")]
            Runner::Remote { url, .. } => format!("remote {}", url),
        }
    }

    fn is_remote(&self) -> bool {
        !matches!(self, Runner::Local(_))
    }

    fn query(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<Value>>), String> {
        match self {
            Runner::Local(db) => db.query_with_columns(sql, []).map_err(|e| e.to_string()),
            #[cfg(feature = "auth")]
            Runner::Remote { url, token } => {
                let body = Json::Object(vec![
                    ("sql".to_string(), Json::Str(sql.to_string())),
                    ("params".to_string(), Json::Array(vec![])),
                ])
                .to_json();
                let (status, resp) = http_post(url, "/query", &body, Some(token))?;
                if status != 200 {
                    return Err(error_from_json(&resp));
                }
                let parsed =
                    Json::parse(&resp).map_err(|e| format!("bad server response: {}", e))?;
                let cols: Vec<String> = parsed
                    .get("columns")
                    .and_then(|c| c.as_array())
                    .map(|a| {
                        a.iter()
                            .map(|v| v.as_str().unwrap_or("?").to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                let rows: Vec<Vec<Value>> = parsed
                    .get("rows")
                    .and_then(|r| r.as_array())
                    .map(|a| {
                        a.iter()
                            .map(|row| {
                                row.as_array()
                                    .map(|cells| {
                                        cells
                                            .iter()
                                            .map(|c| c.to_value().unwrap_or(Value::Null))
                                            .collect()
                                    })
                                    .unwrap_or_default()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok((cols, rows))
            }
        }
    }

    fn execute(&mut self, sql: &str) -> Result<(), String> {
        match self {
            Runner::Local(db) => db.execute(sql, []).map_err(|e| e.to_string()),
            #[cfg(feature = "auth")]
            Runner::Remote { url, token } => {
                let body = Json::Object(vec![
                    ("sql".to_string(), Json::Str(sql.to_string())),
                    ("params".to_string(), Json::Array(vec![])),
                ])
                .to_json();
                let (status, resp) = http_post(url, "/execute", &body, Some(token))?;
                if status != 200 {
                    return Err(error_from_json(&resp));
                }
                Ok(())
            }
        }
    }

    /// Total row changes (for import reporting); remote always reports 0.
    fn total_changes(&self) -> i64 {
        match self {
            Runner::Local(db) => db.total_changes(),
            #[cfg(feature = "auth")]
            Runner::Remote { .. } => 0,
        }
    }

    /// Physical backup image (local only).
    fn backup_image(&mut self) -> Result<Vec<u8>, String> {
        match self {
            Runner::Local(db) => db.image().map_err(|e| e.to_string()),
            #[cfg(feature = "auth")]
            Runner::Remote { .. } => {
                Err(".backup needs a local database file (remote sessions cannot copy the server's disk image)".to_string())
            }
        }
    }

    /// Open a remote session with the full SCRAM-SHA-256 handshake,
    /// verifying the server's signature (mutual authentication).
    #[cfg(feature = "auth")]
    fn connect_remote(
        url: &str,
        user: &str,
        password_flag: Option<&str>,
    ) -> Result<Runner, String> {
        let password = rustqlite::auth::read_password_interactive(
            password_flag,
            &format!("password for {} at {}: ", user, url),
        )
        .map_err(|e| e.0)?;
        let cx = ClientExchange::start(user);
        let start_body = Json::Object(vec![
            ("username".to_string(), Json::Str(user.to_string())),
            (
                "client_nonce".to_string(),
                Json::Str(cx.client_nonce_hex().to_string()),
            ),
        ])
        .to_json();
        let (status, resp) = http_post(url, "/auth/start", &start_body, None)?;
        if status != 200 {
            return Err(error_from_json(&resp));
        }
        let parsed = Json::parse(&resp).map_err(|e| format!("bad server response: {}", e))?;
        let challenge = rustqlite::auth::Challenge {
            nonce_hex: parsed
                .get("nonce")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            salt_hex: parsed
                .get("salt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            iterations: parsed
                .get("iterations")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .max(0) as u32,
        };
        let session = parsed
            .get("session")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let (client_final, proof, expected_server_sig) =
            cx.finish(&password, &challenge).map_err(|e| e.0)?;
        let finish_body = Json::Object(vec![
            ("session".to_string(), Json::Str(session)),
            ("client_final".to_string(), Json::Str(client_final)),
            ("proof".to_string(), Json::Str(encode_hex(&proof))),
        ])
        .to_json();
        let (status, resp) = http_post(url, "/auth/finish", &finish_body, None)?;
        if status != 200 {
            return Err(error_from_json(&resp));
        }
        let parsed = Json::parse(&resp).map_err(|e| format!("bad server response: {}", e))?;
        let token = parsed
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if token.is_empty() {
            return Err("server returned no token".to_string());
        }
        // Mutual authentication: verify the server's signature.
        let server_sig = parsed
            .get("server_signature")
            .and_then(|v| v.as_str())
            .and_then(decode_hex_ref)
            .unwrap_or_default();
        if server_sig != expected_server_sig.to_vec() {
            return Err(
                "server signature verification failed — possible man-in-the-middle".to_string(),
            );
        }
        Ok(Runner::Remote {
            url: normalize_url(url),
            token,
        })
    }
}

#[cfg(feature = "auth")]
fn decode_hex_ref(s: &str) -> Option<Vec<u8>> {
    decode_hex(s)
}

#[cfg(not(feature = "auth"))]
impl Runner {
    #[allow(dead_code)]
    fn unused(&self) {}
}

/// Normalize `host:port` / `http://host:port` to a bare `host:port` base.
#[cfg(feature = "auth")]
fn normalize_url(url: &str) -> String {
    let trimmed = url
        .strip_prefix("http://")
        .unwrap_or(url)
        .trim_end_matches('/');
    if trimmed.contains(':') {
        trimmed.to_string()
    } else {
        format!("{}:8080", trimmed)
    }
}

fn open_local(path: Option<&str>, sqlite_format: bool) -> Result<Runner, String> {
    let path = path.unwrap_or(":memory:");
    let db = if path == ":memory:" {
        Database::open_in_memory()
    } else if sqlite_format {
        Database::open_sqlite_format(path)
    } else {
        Database::open(path)
    }
    .map_err(|e| format!("error opening {}: {}", path, e))?;
    Ok(Runner::Local(Box::new(db)))
}

// ---------------------------------------------------------------------------
// Minimal HTTP/1.1 client (std only — no TLS; the wire is exactly the
// server's plaintext JSON dialect)
// ---------------------------------------------------------------------------

#[cfg(feature = "auth")]
fn http_post(
    url: &str,
    path: &str,
    body: &str,
    token: Option<&str>,
) -> Result<(u16, String), String> {
    use std::net::TcpStream;
    let base = normalize_url(url);
    let host_port = base.rsplit_once(':').ok_or("bad --connect URL")?;
    let (host, port) = (host_port.0, host_port.1.parse::<u16>().unwrap_or(8080));
    let mut stream = TcpStream::connect((host, port))
        .map_err(|e| format!("cannot connect to {}: {}", base, e))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(30)))
        .ok();
    let mut req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        path, base, body.len()
    );
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {}\r\n", t));
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write failed: {}", e))?;
    let mut resp = Vec::new();
    stream
        .read_to_end(&mut resp)
        .map_err(|e| format!("read failed: {}", e))?;
    let text = String::from_utf8_lossy(&resp).to_string();
    // Status line: "HTTP/1.1 200 OK"
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| "malformed HTTP response".to_string())?;
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Ok((status, body))
}

#[cfg(feature = "auth")]
fn error_from_json(body: &str) -> String {
    Json::parse(body)
        .ok()
        .and_then(|j| {
            j.get("error")
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| body.to_string())
}

// ---------------------------------------------------------------------------
// Dot commands
// ---------------------------------------------------------------------------

fn handle_dot_command(
    runner: &mut Runner,
    line: &str,
    mode: &mut OutputMode,
    out: &mut impl Write,
) -> Result<(), String> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(());
    }
    match parts[0] {
        ".help" => {
            writeln!(out, "Commands:").unwrap();
            writeln!(out, "  .help                Show this help.").unwrap();
            writeln!(out, "  .tables              List all tables and views.").unwrap();
            writeln!(
                out,
                "  .schema [pattern]    Show CREATE statements (filtered by pattern)."
            )
            .unwrap();
            writeln!(
                out,
                "  .dump [FILE]         Full SQL dump: schema + data (default: stdout)."
            )
            .unwrap();
            writeln!(out, "  .export-schema [FILE]  CREATE statements only.").unwrap();
            writeln!(out, "  .export-data [FILE]    INSERT statements only.").unwrap();
            writeln!(out, "  .read FILE           Execute an SQL script.").unwrap();
            writeln!(
                out,
                "  .import FILE [TABLE] Execute an SQL script, or import CSV into TABLE."
            )
            .unwrap();
            writeln!(
                out,
                "  .backup FILE         Physical byte-copy backup of the database."
            )
            .unwrap();
            writeln!(out, "  .mode json|table|csv|line  Output mode.").unwrap();
            writeln!(out, "  .quit / .exit        Exit the shell.").unwrap();
        }
        ".tables" => {
            let (_, rows) = runner.query(
                "SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )?;
            for row in rows {
                if let Some(Value::Text(n)) = row.first() {
                    writeln!(out, "{}", n.as_str()).unwrap();
                }
            }
        }
        ".schema" => {
            let pattern = parts.get(1).copied();
            let sql = match pattern {
                Some(p) => format!(
                    "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' AND (name LIKE '{}' OR tbl_name LIKE '{}') ORDER BY rowid",
                    p, p
                ),
                None => "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid".to_string(),
            };
            let (_, rows) = runner.query(&sql)?;
            for row in rows {
                if let Some(Value::Text(s)) = row.first() {
                    writeln!(out, "{};", s.as_str()).unwrap();
                }
            }
        }
        ".dump" => {
            let target = parts.get(1).copied();
            let dump = build_full_dump(runner)?;
            write_output(out, target, &dump)?;
        }
        ".export-schema" => {
            let target = parts.get(1).copied();
            let schema = build_schema_dump(runner)?;
            write_output(out, target, &schema)?;
        }
        ".export-data" => {
            let target = parts.get(1).copied();
            let data = build_data_dump(runner)?;
            write_output(out, target, &data)?;
        }
        ".read" | ".import" => {
            let file = parts
                .get(1)
                .ok_or_else(|| format!("usage: {} FILE [TABLE]", parts[0]))?;
            match parts.get(2) {
                Some(table) => import_csv(runner, file, table, out)?,
                None => {
                    let changes_before = runner.total_changes();
                    let script = std::fs::read_to_string(file)
                        .map_err(|e| format!("cannot read {}: {}", file, e))?;
                    runner.execute(&script)?;
                    let delta = runner.total_changes() - changes_before;
                    if delta > 0 {
                        writeln!(out, "OK ({} row changes)", delta).unwrap();
                    } else {
                        writeln!(out, "OK").unwrap();
                    }
                }
            }
        }
        ".backup" => {
            let file = parts
                .get(1)
                .ok_or_else(|| "usage: .backup FILE".to_string())?;
            let image = runner.backup_image()?;
            std::fs::write(file, &image).map_err(|e| format!("cannot write {}: {}", file, e))?;
            writeln!(out, "backup written: {} ({} bytes)", file, image.len()).unwrap();
        }
        ".mode" => {
            if parts.len() >= 2 {
                *mode = match parts[1] {
                    "json" => OutputMode::Json,
                    "table" => OutputMode::Table,
                    "csv" => OutputMode::Csv,
                    "line" => OutputMode::Line,
                    _ => {
                        writeln!(out, "unknown mode: {}", parts[1]).unwrap();
                        return Ok(());
                    }
                };
            } else {
                writeln!(out, "current mode: {:?}", mode).unwrap();
            }
        }
        ".quit" | ".exit" => {
            std::process::exit(0);
        }
        _ => {
            writeln!(out, "unknown command: {} (try .help)", parts[0]).unwrap();
        }
    }
    Ok(())
}

/// Write to stdout or to a file (`.dump backup.sql`); `-` means stdout.
fn write_output(out: &mut impl Write, target: Option<&str>, content: &str) -> Result<(), String> {
    match target {
        None | Some("-") => {
            write!(out, "{}", content).unwrap();
            Ok(())
        }
        Some(file) => {
            std::fs::write(file, content).map_err(|e| format!("cannot write {}: {}", file, e))?;
            println!("written: {} ({} bytes)", file, content.len());
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Batch operations
// ---------------------------------------------------------------------------

fn run_batch_op(op: &BatchOp, runner: &mut Runner, out: &mut impl Write) -> i32 {
    match run_batch_op_inner(op, runner, out) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}

fn run_batch_op_inner(
    op: &BatchOp,
    runner: &mut Runner,
    out: &mut impl Write,
) -> Result<(), String> {
    match op {
        BatchOp::Dump(target) => {
            let dump = build_full_dump(runner)?;
            write_output(out, target.as_deref(), &dump)
        }
        BatchOp::ExportSchema(target) => {
            let schema = build_schema_dump(runner)?;
            write_output(out, target.as_deref(), &schema)
        }
        BatchOp::ExportData(target) => {
            let data = build_data_dump(runner)?;
            write_output(out, target.as_deref(), &data)
        }
        BatchOp::Backup(file) => {
            let image = runner.backup_image()?;
            std::fs::write(file, &image).map_err(|e| format!("cannot write {}: {}", file, e))?;
            println!("backup written: {} ({} bytes)", file, image.len());
            Ok(())
        }
        BatchOp::Import(file) => {
            let changes_before = runner.total_changes();
            let script = std::fs::read_to_string(file)
                .map_err(|e| format!("cannot read {}: {}", file, e))?;
            runner.execute(&script)?;
            let delta = runner.total_changes() - changes_before;
            if delta > 0 {
                println!("OK ({} row changes)", delta);
            } else {
                println!("OK");
            }
            Ok(())
        }
        BatchOp::ImportCsv(file, table) => import_csv(runner, file, table, out),
    }
}

// ---------------------------------------------------------------------------
// Dump machinery (pure SQL over the Runner — works locally AND remotely)
// ---------------------------------------------------------------------------

/// One sqlite_master row we care about.
struct DbObject {
    name: String,
    sql: String,
}

fn sqlite_master_objects(runner: &Runner, types: &str) -> Result<Vec<DbObject>, String> {
    let sql = format!(
        "SELECT type, name, sql FROM sqlite_master WHERE type IN ({}) AND sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid",
        types
    );
    let (_, rows) = runner.query(&sql)?;
    let mut out = Vec::new();
    for row in rows {
        let name = match row.get(1) {
            Some(Value::Text(t)) => t.as_str().to_string(),
            _ => continue,
        };
        let ddl = match row.get(2) {
            Some(Value::Text(t)) => t.as_str().to_string(),
            _ => continue,
        };
        out.push(DbObject { name, sql: ddl });
    }
    Ok(out)
}

/// Does `sqlite_master` contain a given internal (`sqlite_*`) table?
fn internal_table_exists(runner: &Runner, name: &str) -> Result<bool, String> {
    let sql = format!(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '{}'",
        name
    );
    let (_, rows) = runner.query(&sql)?;
    Ok(matches!(
        rows.first().and_then(|r| r.first()),
        Some(Value::Integer(n)) if *n > 0
    ))
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Render a [`Value`] as a SQL literal (SQLite `.dump`'s rules).
fn sql_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => {
            if f.is_nan() {
                // SQLite's own dump renders NaN as NULL.
                "NULL".to_string()
            } else if *f == f64::INFINITY {
                "9.0e999".to_string()
            } else if *f == f64::NEG_INFINITY {
                "-9.0e999".to_string()
            } else {
                let s = format!("{}", f);
                // Keep the REAL shape (Rust prints 5.0 as "5"; "5" would
                // re-import as INTEGER under text/blob affinity).
                if s.contains('.') || s.contains('e') || s.contains('E') {
                    s
                } else {
                    format!("{}.0", s)
                }
            }
        }
        Value::Text(t) => {
            if t.as_bytes().contains(&0) {
                // Embedded NUL cannot ride in a SQL string literal.
                format!("X'{}'", hex_encode(t.as_bytes()))
            } else {
                format!("'{}'", t.as_str().replace('\'', "''"))
            }
        }
        Value::Blob(b) => format!("X'{}'", hex_encode(b)),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// `INSERT INTO "t" VALUES(...);` lines for one table's data.
fn table_data_stmts(runner: &Runner, table: &str) -> Result<Vec<String>, String> {
    let sql = format!("SELECT * FROM {}", quote_ident(table));
    let (_, rows) = runner.query(&sql)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let values: Vec<String> = row.iter().map(sql_literal).collect();
        out.push(format!(
            "INSERT INTO {} VALUES({});",
            quote_ident(table),
            values.join(",")
        ));
    }
    Ok(out)
}

/// Rows of an internal bookkeeping table (`sqlite_sequence`, ...),
/// rendered with a leading DELETE.
fn internal_table_stmts(runner: &Runner, table: &str) -> Result<Vec<String>, String> {
    if !internal_table_exists(runner, table)? {
        return Ok(Vec::new());
    }
    let mut out = vec![format!("DELETE FROM {};", quote_ident(table))];
    let sql = format!("SELECT * FROM {}", quote_ident(table));
    let (_, rows) = runner.query(&sql)?;
    for row in rows {
        let values: Vec<String> = row.iter().map(sql_literal).collect();
        out.push(format!(
            "INSERT INTO {} VALUES({});",
            quote_ident(table),
            values.join(",")
        ));
    }
    Ok(out)
}

/// Schema section: table DDL first (creation order), then
/// indexes/views/triggers AFTER all table DDL — so a dump can be
/// re-imported without triggers firing on the data load and with
/// every object defined after the tables it references.
fn schema_sections(runner: &Runner) -> Result<(Vec<String>, Vec<String>), String> {
    let tables = sqlite_master_objects(runner, "'table'")?;
    let others = sqlite_master_objects(runner, "'index','view','trigger'")?;
    let table_ddl: Vec<String> = tables.iter().map(|t| format!("{};", t.sql)).collect();
    let other_ddl: Vec<String> = others.iter().map(|t| format!("{};", t.sql)).collect();
    Ok((table_ddl, other_ddl))
}

/// Data section: every user table's INSERTs (table order), then the
/// bookkeeping tables (`sqlite_sequence` preserves AUTOINCREMENT
/// high-water marks across a dump/load cycle).
fn data_sections(runner: &Runner) -> Result<Vec<String>, String> {
    let tables = sqlite_master_objects(runner, "'table'")?;
    let mut out = Vec::new();
    for t in &tables {
        if t.sql.to_uppercase().starts_with("CREATE VIRTUAL TABLE") {
            // Virtual table data is module-defined; try the SELECT and
            // note any failure instead of failing the whole dump.
            match table_data_stmts(runner, &t.name) {
                Ok(stmts) => out.extend(stmts),
                Err(e) => out.push(format!(
                    "-- skipped data for virtual table {}: {}",
                    t.name, e
                )),
            }
        } else {
            out.extend(table_data_stmts(runner, &t.name)?);
        }
    }
    out.extend(internal_table_stmts(runner, "sqlite_sequence")?);
    out.extend(internal_table_stmts(runner, "sqlite_stat1")?);
    Ok(out)
}

/// Full `.dump`: PRAGMA + BEGIN + schema + data + COMMIT.
fn build_full_dump(runner: &Runner) -> Result<String, String> {
    let (table_ddl, other_ddl) = schema_sections(runner)?;
    let data = data_sections(runner)?;
    let mut out = String::new();
    out.push_str("PRAGMA foreign_keys=OFF;\n");
    out.push_str("BEGIN TRANSACTION;\n");
    for ddl in table_ddl {
        out.push_str(&ddl);
        out.push('\n');
    }
    for stmt in &data {
        out.push_str(stmt);
        out.push('\n');
    }
    for ddl in other_ddl {
        out.push_str(&ddl);
        out.push('\n');
    }
    out.push_str("COMMIT;\n");
    Ok(out)
}

/// `.export-schema`: the CREATE statements only.
fn build_schema_dump(runner: &Runner) -> Result<String, String> {
    let (table_ddl, other_ddl) = schema_sections(runner)?;
    let mut out = String::new();
    for ddl in table_ddl.into_iter().chain(other_ddl) {
        out.push_str(&ddl);
        out.push('\n');
    }
    Ok(out)
}

/// `.export-data`: the INSERT statements only.
fn build_data_dump(runner: &Runner) -> Result<String, String> {
    let data = data_sections(runner)?;
    let mut out = String::new();
    for stmt in data {
        out.push_str(&stmt);
        out.push('\n');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// CSV import (sqlite3's `.import FILE TABLE` semantics)
// ---------------------------------------------------------------------------

/// RFC 4180-ish CSV: comma-separated, `"`-quoted fields with `""`
/// escapes, newlines inside quotes, `\r\n` line endings tolerated.
fn parse_csv(text: &str) -> Result<Vec<Vec<String>>, String> {
    let b = text.as_bytes();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut i = 0;
    let mut line_no = 1usize;
    while i < b.len() {
        let c = b[i];
        if in_quotes {
            if c == b'"' {
                if i + 1 < b.len() && b[i + 1] == b'"' {
                    field.push('"');
                    i += 2;
                } else {
                    in_quotes = false;
                    i += 1;
                }
            } else {
                if c == b'\n' {
                    line_no += 1;
                }
                // Copy the raw byte (UTF-8 sequences pass through).
                field.push(c as char);
                i += 1;
            }
            continue;
        }
        match c {
            b'"' => {
                if field.is_empty() {
                    in_quotes = true;
                    i += 1;
                } else {
                    // A quote mid-field (unquoted field): literal.
                    field.push('"');
                    i += 1;
                }
            }
            b',' => {
                row.push(std::mem::take(&mut field));
                i += 1;
            }
            b'\r' => {
                i += 1;
                if i < b.len() && b[i] == b'\n' {
                    i += 1;
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                line_no += 1;
            }
            b'\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                line_no += 1;
                i += 1;
            }
            c => {
                // ASCII fast path; multi-byte UTF-8 needs whole sequences.
                if c < 0x80 {
                    field.push(c as char);
                    i += 1;
                } else {
                    let len = match c {
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf7 => 4,
                        _ => 1,
                    };
                    let end = (i + len).min(b.len());
                    if let Ok(s) = std::str::from_utf8(&b[i..end]) {
                        field.push_str(s);
                        i = end;
                    } else {
                        field.push('\u{fffd}');
                        i += 1;
                    }
                }
            }
        }
    }
    // Final field/row without a trailing newline.
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    let _ = line_no;
    Ok(rows)
}

/// Import a CSV file into `table`. When the table does not exist it is
/// created from the header row with TEXT columns (sqlite3's rule); when
/// it exists, every row is data (no header skip) and the field count
/// must match the column count.
fn import_csv(
    runner: &mut Runner,
    file: &str,
    table: &str,
    out: &mut impl Write,
) -> Result<(), String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read {}: {}", file, e))?;
    let rows = parse_csv(&text)?;
    if rows.is_empty() {
        writeln!(out, "(empty CSV: {})", file).unwrap();
        return Ok(());
    }
    let exists = {
        let sql = format!(
            "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view') AND name = '{}'",
            table.replace('\'', "''")
        );
        let (_, r) = runner.query(&sql)?;
        matches!(r.first().and_then(|row| row.first()), Some(Value::Integer(n)) if *n > 0)
    };
    let (header, data_rows) = if exists {
        (None, rows)
    } else {
        // Create from the header row; all columns TEXT (sqlite3 behavior).
        let header: Vec<String> = rows[0]
            .iter()
            .map(|h| quote_ident(&sanitize_column_name(h)))
            .collect();
        let ddl = format!(
            "CREATE TABLE {} ({});",
            quote_ident(table),
            header
                .iter()
                .map(|h| format!("{} TEXT", h))
                .collect::<Vec<_>>()
                .join(", ")
        );
        runner.execute(&ddl)?;
        (Some(()), rows[1..].to_vec())
    };
    let _ = header;

    // Column count: derive from the table itself (NOT the CSV), so an
    // existing table defines the expectation like sqlite3.
    let expected = {
        let sql = format!("SELECT * FROM {} WHERE 0", quote_ident(table));
        let (cols, _) = runner.query(&sql)?;
        cols.len()
    };
    if expected == 0 {
        return Err(format!("table {} has no columns", table));
    }

    let mut n = 0usize;
    for (idx, row) in data_rows.iter().enumerate() {
        if row.len() != expected {
            return Err(format!(
                "{}: row {}: expected {} columns but found {}",
                file,
                idx + 1,
                expected,
                row.len()
            ));
        }
        let values: Vec<String> = row
            .iter()
            .map(|v| format!("'{}'", v.replace('\'', "''")))
            .collect();
        let insert = format!(
            "INSERT INTO {} VALUES({});",
            quote_ident(table),
            values.join(",")
        );
        runner
            .execute(&insert)
            .map_err(|e| format!("{}: row {}: {}", file, idx + 1, e))?;
        n += 1;
    }
    if exists {
        writeln!(out, "imported {} rows into {}", n, table).unwrap();
    } else {
        writeln!(
            out,
            "created {} ({} TEXT columns) and imported {} rows",
            table, expected, n
        )
        .unwrap();
    }
    Ok(())
}

/// CSV header names may be arbitrary strings; make them safe identifiers.
fn sanitize_column_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "c".to_string()
    } else if cleaned.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("c{}", cleaned)
    } else {
        cleaned
    }
}

// ---------------------------------------------------------------------------
// SQL execution + output modes
// ---------------------------------------------------------------------------

fn execute_sql(
    runner: &mut Runner,
    sql: &str,
    mode: &OutputMode,
    out: &mut impl Write,
) -> Result<(), String> {
    // Determine if this is a query or a DML/DDL statement by the leading
    // keyword (the remote path needs the split to pick /query vs /execute;
    // locally it only decides whether to print rows).
    let trimmed = sql.trim_start();
    let is_query = starts_with_keyword(
        trimmed,
        &["SELECT", "WITH", "VALUES", "EXPLAIN", "PRAGMA", "TABLE"],
    );

    if is_query {
        let (cols, rows) = runner.query(sql)?;
        print_rows(out, &cols, &rows, mode);
    } else {
        runner.execute(sql)?;
        writeln!(out, "OK").unwrap();
    }
    Ok(())
}

fn starts_with_keyword(text: &str, keywords: &[&str]) -> bool {
    for kw in keywords {
        let k = text.len() >= kw.len();
        if k && text[..kw.len()].eq_ignore_ascii_case(kw) {
            // Must be followed by whitespace, '(', or end.
            let rest = &text[kw.len()..];
            if rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('(') {
                return true;
            }
        }
    }
    false
}

#[derive(Clone, Copy, Debug)]
enum OutputMode {
    Table,
    Json,
    Csv,
    Line,
}

fn print_rows(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>], mode: &OutputMode) {
    if rows.is_empty() {
        writeln!(out, "(no rows)").unwrap();
        return;
    }
    match mode {
        OutputMode::Table => print_table(out, cols, rows),
        OutputMode::Json => print_json(out, cols, rows),
        OutputMode::Csv => print_csv(out, cols, rows),
        OutputMode::Line => print_line(out, cols, rows),
    }
}

fn print_table(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>]) {
    // Compute column widths.
    let mut widths: Vec<usize> = cols.iter().map(|c| c.len()).collect();
    for row in rows {
        for (i, v) in row.iter().enumerate() {
            let len = format_value(v).len();
            if i < widths.len() && len > widths[i] {
                widths[i] = len;
            }
        }
    }
    // Header
    let header: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{:width$}", c, width = widths[i]))
        .collect();
    writeln!(out, "| {} |", header.join(" | ")).unwrap();
    // Separator
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    writeln!(out, "|-{}-|", sep.join("-|-")).unwrap();
    // Rows
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let s = format_value(v);
                format!("{:width$}", s, width = widths.get(i).copied().unwrap_or(0))
            })
            .collect();
        writeln!(out, "| {} |", cells.join(" | ")).unwrap();
    }
    writeln!(out, "({} rows)", rows.len()).unwrap();
}

fn print_json(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>]) {
    writeln!(out, "[").unwrap();
    for (i, row) in rows.iter().enumerate() {
        write!(out, "  {{").unwrap();
        for (j, v) in row.iter().enumerate() {
            if j > 0 {
                write!(out, ", ").unwrap();
            }
            let col_name = cols.get(j).map(|s| s.as_str()).unwrap_or("?");
            write!(out, "{:?}: {}", col_name, json_value(v)).unwrap();
        }
        write!(out, "}}").unwrap();
        if i < rows.len() - 1 {
            write!(out, ",").unwrap();
        }
        writeln!(out).unwrap();
    }
    writeln!(out, "]").unwrap();
}

fn print_csv(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>]) {
    let _ = cols;
    for row in rows {
        let cells: Vec<String> = row.iter().map(format_value).collect();
        writeln!(out, "{}", cells.join(",")).unwrap();
    }
}

fn print_line(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>]) {
    let max_col_len = cols.iter().map(|c| c.len()).max().unwrap_or(0);
    for row in rows {
        for (i, v) in row.iter().enumerate() {
            let name = cols.get(i).map(|s| s.as_str()).unwrap_or("?");
            writeln!(
                out,
                "{:>width$} = {}",
                name,
                format_value(v),
                width = max_col_len
            )
            .unwrap();
        }
        writeln!(out).unwrap();
    }
}

fn format_value(v: &Value) -> String {
    match v {
        Value::Null => "".to_string(),
        _ => format!("{}", v),
    }
}

fn json_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => f.to_string(),
        Value::Text(s) => format!("{:?}", s.as_str()),
        Value::Blob(b) => {
            let hex: String = b.iter().map(|x| format!("{:02x}", x)).collect();
            format!("\"{}\"", hex)
        }
    }
}
