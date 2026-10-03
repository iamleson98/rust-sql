//! Interactive CLI shell for rustqlite.
//!
//! Usage:
//!   rustqlite-cli [OPTIONS] [DB_PATH]
//!
//! Database options:
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

mod archive;
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
    ExportSqlite(String),
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
    println!("  --export-sqlite FILE    Write a REAL SQLite-format copy");
    println!("  --import FILE           Execute an SQL script");
    println!("  --import-csv FILE TABLE Load CSV into TABLE");
    println!();
    println!("In the shell, .help lists dot commands.");
}

fn main() {
    let args: Vec<String> = env::args().collect();
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
            "--export-sqlite" => match value(&mut i) {
                Some(v) => batch = Some(BatchOp::ExportSqlite(v)),
                None => {
                    eprintln!("error: --export-sqlite needs a FILE argument");
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
        open_local(path.as_deref())
    };
    #[cfg(not(feature = "auth"))]
    let runner: Result<Runner, String> = if connect.is_some() {
        Err(
            "this build lacks the `auth` feature; rebuild with --features auth for --connect"
                .to_string(),
        )
    } else {
        open_local(path.as_deref())
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
    let mut state = ShellState::default();
    let mut out = ShellOut::stdout();
    // `.output`'s persistent handle. It is opened (truncated) once when
    // the directive names a NEW file and then kept open: sqlite3 never
    // re-truncates the redirect target between statements, so a naive
    // `File::create` per statement would wipe everything but the last
    // statement's output. A `.once` interlude borrows a different handle
    // and returns this one untouched.
    let mut persist_path: Option<String> = None;
    let mut persist_file: Option<std::fs::File> = None;
    // Temp files pending deletion (`.once -e/-x|-w` captures: the opener
    // keeps the file for ~10s; failed openers delete on the next pass,
    // like sqlite3's aUnlink queue).
    let mut unlink_queue: Vec<(std::time::Instant, String)> = Vec::new();
    // `.once -w` prefix/suffix bookkeeping (the HTML skeleton).
    let mut www_suffix_pending: Option<String> = None;

    loop {
        let main_prompt = if runner.is_remote() {
            "rustqlite-remote".to_string()
        } else {
            state.prompt_main.clone()
        };
        let prompt = if buffer.is_empty() {
            main_prompt
        } else {
            state.prompt_cont.clone()
        };
        // Prompts always go to the console, even while output is
        // redirected (sqlite3 keeps the prompt on screen).
        write!(stdout, "{}> ", prompt).unwrap();
        let _ = stdout.flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break; // EOF
        }
        let line = line.trim();
        if buffer.is_empty() && line.starts_with('.') {
            // Dot-command output follows a persistent `.output` (as in
            // sqlite3); `.once` does not apply to dot-commands. The
            // writer is resolved BEFORE the directive runs, so the
            // `.output` line itself reports to the previous target.
            sync_persist(
                &mut persist_path,
                &mut persist_file,
                state.output_file.as_deref(),
            );
            let mut persist_in_out = false;
            if let Some(fh) = persist_file.take() {
                let _ = out.flush();
                out = ShellOut::File(fh);
                persist_in_out = true;
            } else if !matches!(out, ShellOut::Stdout(_)) {
                let _ = out.flush();
                out = ShellOut::stdout();
            }
            if let Err(e) = handle_dot_command(&mut runner, line, &mut state, &mut out) {
                eprintln!("error: {}", e);
            }
            // Hand the handle back so the next unit reuses it.
            if persist_in_out {
                let _ = out.flush();
                if let ShellOut::File(fh) = std::mem::replace(&mut out, ShellOut::stdout()) {
                    persist_file = Some(fh);
                }
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
            // Resolve this statement's writer: `.once` (a fresh file,
            // this statement only) takes priority; else the persistent
            // `.output` handle; else stdout.
            sync_persist(
                &mut persist_path,
                &mut persist_file,
                state.output_file.as_deref(),
            );
            let once_here = state.once_file.is_some();
            let special_here = state.once_special.is_some();
            let mut persist_in_out = false;
            if let Some(sp) = state.once_special.as_ref() {
                // `.once -e/-x|-w`: capture to the generated temp file.
                let _ = out.flush();
                out = match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&sp.path)
                {
                    Ok(fh) => ShellOut::File(fh),
                    Err(e) => {
                        eprintln!("error: cannot open {}: {}", sp.path, e);
                        ShellOut::stdout()
                    }
                };
                if let OnceKind::Browser { plain: false } = sp.kind {
                    // The non-plain browser capture wraps the rows in a
                    // table skeleton (sqlite3's MODE_Www).
                    let _ = write!(
                        out,
                        "</PRE>\n<TABLE border='1' cellspacing='0' cellpadding='2'>\n"
                    );
                    www_suffix_pending = Some("</TABLE>\n<PRE>".to_string());
                }
            } else if let Some(p) = state.once_file.clone() {
                let _ = out.flush();
                out = match std::fs::File::create(&p) {
                    Ok(fh) => ShellOut::File(fh),
                    Err(e) => {
                        eprintln!("error: cannot open {}: {}", p, e);
                        ShellOut::stdout()
                    }
                };
            } else if let Some(fh) = persist_file.take() {
                let _ = out.flush();
                out = ShellOut::File(fh);
                persist_in_out = true;
            } else if !matches!(out, ShellOut::Stdout(_)) {
                let _ = out.flush();
                out = ShellOut::stdout();
            }
            if let Err(e) = execute_sql(&mut runner, &sql, &state, &mut out) {
                eprintln!("error: {}", e);
                log_shell_line(&mut state, 1, &format!("{} in \"{}\"", e, sql));
            }
            if special_here {
                // `.once -e|-x|-w`: finish the capture — close the temp
                // file, hand it to the system opener, restore the shell
                // settings (sqlite3's modePush/modePop + doXdgOpen).
                if let Some(suffix) = www_suffix_pending.take() {
                    let _ = write!(out, "{}", suffix);
                    let _ = writeln!(out, "</PRE></BODY></HTML>");
                }
                let sp = state.once_special.take().expect("special_here");
                let _ = out.flush();
                out = ShellOut::stdout();
                let ok = open_with_system_opener(&sp.path);
                unlink_queue.push((
                    std::time::Instant::now()
                        + if ok {
                            std::time::Duration::from_secs(10)
                        } else {
                            std::time::Duration::ZERO
                        },
                    sp.path.clone(),
                ));
                // Restore the saved shell settings.
                let saved = sp.saved;
                state.mode = saved.mode;
                state.col_sep = saved.col_sep;
                state.row_sep = saved.row_sep;
                state.headers = saved.headers;
                state.csv_unquoted = false;
            } else if once_here {
                // `.once` is consumed. A still-active `.output` resumes
                // on the next unit — its handle was never touched, so
                // the file keeps everything written before the interlude.
                state.once_file = None;
                let _ = out.flush();
                out = ShellOut::stdout();
            } else if persist_in_out {
                let _ = out.flush();
                if let ShellOut::File(fh) = std::mem::replace(&mut out, ShellOut::stdout()) {
                    persist_file = Some(fh);
                }
            }
        }
        // Expire due temp captures (sqlite3's aUnlink processing).
        let now = std::time::Instant::now();
        unlink_queue.retain(|(at, path)| {
            if now >= *at {
                let _ = std::fs::remove_file(path);
                false
            } else {
                true
            }
        });
    }
    println!();
}

/// Write one line to the `.log` target in sqlite3_log's format
/// ("(code) message"). No-op when no log is open.
fn log_shell_line(state: &mut ShellState, code: i32, msg: &str) {
    use std::io::Write;
    if let Some(f) = state.log_file.as_mut() {
        let _ = writeln!(f, "({}) {}", code, msg);
        let _ = f.flush();
    }
}

/// Reconcile the persistent `.output` handle with the current directive:
/// a changed path closes the old handle and opens the new file (empty);
/// an unchanged path keeps the handle open. Opening failures are
/// reported once (the path is remembered so we do not retry every unit).
fn sync_persist(
    persist_path: &mut Option<String>,
    persist_file: &mut Option<std::fs::File>,
    want: Option<&str>,
) {
    if persist_path.as_deref() == want {
        return;
    }
    *persist_file = None; // close the old handle
    *persist_path = want.map(str::to_string);
    if let Some(p) = persist_path.as_deref() {
        *persist_file = match std::fs::File::create(p) {
            Ok(fh) => Some(fh),
            Err(e) => {
                eprintln!("error: cannot open {}: {}", p, e);
                None
            }
        };
    }
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

    fn changes(&self) -> i64 {
        match self {
            Runner::Local(db) => db.changes(),
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

    /// Write a REAL SQLite-format copy of the local database (the
    /// interchange writer — the file opens in the sqlite3 CLI).
    fn export_sqlite_image(&mut self, file: &str) -> Result<(), String> {
        match self {
            Runner::Local(db) => db
                .export_sqlite_format(file)
                .map_err(|e| format!("cannot write {}: {}", file, e)),
            #[cfg(feature = "auth")]
            Runner::Remote { .. } => Err(
                "--export-sqlite needs a local database file (remote sessions cannot export the server's disk image)"
                    .to_string(),
            ),
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

fn open_local(path: Option<&str>) -> Result<Runner, String> {
    let path = path.unwrap_or(":memory:");
    let mut db = if path == ":memory:" {
        Database::open_in_memory()
    } else {
        // A SQLite-format file sniffs and loads for interop; its first
        // write adopts the path into the native container.
        Database::open(path)
    }
    .map_err(|e| format!("error opening {}: {}", path, e))?;
    // The sqlite3 shell links its extension set (shathree) into every
    // session: sha3()/sha3_agg() are available to all local sessions.
    let _ = rustqlite::plugin::sha3::register(&mut db);
    // The shell's file/archive function family (fileio.c): lsmode,
    // realpath, readfile, writefile, shell_putsnl, sqlar_* — used by
    // .archive and available to user SQL like in the sqlite3 shell.
    let _ = rustqlite::plugin::filefns::register(&mut db);
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

/// Session state for the shell (sqlite3's dot-command toggles).
struct ShellState {
    mode: OutputMode,
    /// `.headers on|off` — column headers in table/csv output.
    headers: bool,
    /// `.nullvalue STRING` — how SQL NULL renders.
    nullvalue: String,
    /// `.timer on|off` — per-statement wall time.
    timer: bool,
    /// `.changes on|off` — per-statement change counts.
    changes: bool,
    /// `.echo on|off` — echo each SQL statement before running it.
    echo: bool,
    /// `.eqp on|off` — EXPLAIN QUERY PLAN before each statement.
    eqp: bool,
    /// `.width N...` — fixed table-mode column widths (0 = auto).
    widths: Vec<usize>,
    /// `.separator COL [ROW]` — csv-mode separators.
    col_sep: String,
    row_sep: String,
    /// `.output FILE` — persistent redirection (None = stdout).
    output_file: Option<String>,
    /// `.once FILE` — next statement only.
    once_file: Option<String>,
    /// `.prompt MAIN [CONT]`
    prompt_main: String,
    prompt_cont: String,
    /// `.log FILE|on|off` — the log stream (sqlite3_log lines).
    log_file: Option<std::fs::File>,
    /// `.scanstats on|off|est|vm` — accepted; the engine has no
    /// scan-status counters (same warning as a SQLite build without
    /// SQLITE_ENABLE_STMT_SCANSTATUS).
    scanstats: u8,
    /// `.once -e|-x|-w` / `.excel` / `.www`: capture the next
    /// statement to a temp file and hand it to the system opener.
    once_special: Option<OnceSpecial>,
    /// `.once -x` interlude flag: csv WITHOUT quoting (sqlite3's .excel
    /// quirk — .mode csv quotes, the excel capture does not).
    csv_unquoted: bool,
}

/// A `.once -e|-x|-w` / `.excel` / `.www` capture.
struct OnceSpecial {
    /// The temp file (created under $HOME like sqlite3's temp-RAND.ext).
    path: String,
    kind: OnceKind,
    /// Shell settings to restore after the statement.
    saved: ModeSnapshot,
}

enum OnceKind {
    /// `.once -e` — temp .txt, current mode, opened with the system
    /// opener.
    Editor,
    /// `.once -x` / `.excel` — temp .csv, unquoted csv with CRLF.
    Spreadsheet,
    /// `.once -w` / `.www` — temp .html, HTML tables (or --plain text).
    Browser { plain: bool },
}

struct ModeSnapshot {
    mode: OutputMode,
    col_sep: String,
    row_sep: String,
    headers: bool,
}

impl Default for ShellState {
    fn default() -> Self {
        Self {
            mode: OutputMode::Table,
            headers: true,
            nullvalue: String::new(),
            timer: false,
            changes: false,
            echo: false,
            eqp: false,
            widths: Vec::new(),
            col_sep: ",".to_string(),
            row_sep: "\n".to_string(),
            output_file: None,
            once_file: None,
            prompt_main: "rustqlite".to_string(),
            prompt_cont: "  ...".to_string(),
            log_file: None,
            scanstats: 0,
            once_special: None,
            csv_unquoted: false,
        }
    }
}

/// The shell's writer: stdout (pipe-tolerant) or a redirected file.
enum ShellOut {
    Stdout(PipeTolerant<io::Stdout>),
    File(std::fs::File),
}

impl ShellOut {
    fn stdout() -> Self {
        ShellOut::Stdout(PipeTolerant(io::stdout()))
    }
}

impl Write for ShellOut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            ShellOut::Stdout(w) => w.write(buf),
            ShellOut::File(f) => f.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            ShellOut::Stdout(w) => w.flush(),
            ShellOut::File(f) => f.flush(),
        }
    }
}

/// Every dot command the shell knows (resolution + prefix matching +
/// the ambiguity error text).
const DOT_COMMANDS: [&str; 41] = [
    ".archive",
    ".backup",
    ".changes",
    ".clone",
    ".databases",
    ".dbinfo",
    ".dump",
    ".echo",
    ".eqp",
    ".excel",
    ".exit",
    ".export-data",
    ".export-schema",
    ".fullschema",
    ".headers",
    ".help",
    ".import",
    ".indexes",
    ".log",
    ".mode",
    ".nullvalue",
    ".once",
    ".open",
    ".output",
    ".print",
    ".prompt",
    ".quit",
    ".read",
    ".restore",
    ".save",
    ".scanstats",
    ".schema",
    ".separator",
    ".sha3sum",
    ".shell",
    ".stats",
    ".system",
    ".tables",
    ".timer",
    ".width",
    ".www",
];

/// Exact match, else unique prefix (sqlite3's oneline.c resolution);
/// unknown or ambiguous names resolve to the ORIGINAL text so the
/// dispatch's `_` arm prints the unknown-command line.
fn resolve_command(name: &str) -> String {
    if DOT_COMMANDS.contains(&name) {
        return name.to_string();
    }
    let cands: Vec<&str> = DOT_COMMANDS
        .iter()
        .filter(|c| c.starts_with(name) && name.len() > 1)
        .copied()
        .collect();
    match cands.len() {
        1 => cands[0].to_string(),
        0 => name.to_string(),
        _ => {
            eprintln!(
                "ambiguous command: {} candidates are: {}",
                name,
                cands.join(" ")
            );
            name.to_string()
        }
    }
}

fn handle_dot_command(
    runner: &mut Runner,
    line: &str,
    state: &mut ShellState,
    out: &mut ShellOut,
) -> Result<(), String> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(());
    }
    // sqlite3's dot-command resolution: the EXACT name wins; otherwise
    // a UNIQUE prefix match (`.ar` for `.archive`, `.data` for
    // `.databases`); an ambiguous prefix lists the candidates.
    let resolved: String = resolve_command(parts[0]);
    match resolved.as_str() {
        ".help" => {
            writeln!(out, "Commands:").unwrap();
            writeln!(out, "  .help                     Show this help.").unwrap();
            writeln!(
                out,
                "  .tables [pattern]         List tables and views (LIKE pattern)."
            )
            .unwrap();
            writeln!(
                out,
                "  .indexes [pattern]        List indexes (LIKE pattern)."
            )
            .unwrap();
            writeln!(out, "  .schema [pattern]         Show CREATE statements.").unwrap();
            writeln!(
                out,
                "  .databases                List open databases (incl. ATTACHed)."
            )
            .unwrap();
            writeln!(
                out,
                "  .dbinfo                   Database file header info."
            )
            .unwrap();
            writeln!(out, "  .stats [on|off]           Page/schema statistics.").unwrap();
            writeln!(
                out,
                "  .dump [FILE]              Full SQL dump: schema + data."
            )
            .unwrap();
            writeln!(out, "  .export-schema [FILE]     CREATE statements only.").unwrap();
            writeln!(out, "  .export-data [FILE]       INSERT statements only.").unwrap();
            writeln!(out, "  .read FILE                Execute an SQL script.").unwrap();
            writeln!(
                out,
                "  .import FILE [TABLE]      Run a script, or import CSV into TABLE."
            )
            .unwrap();
            writeln!(
                out,
                "  .backup FILE              Physical byte-copy backup."
            )
            .unwrap();
            writeln!(out, "  .save FILE                Alias for .backup.").unwrap();
            writeln!(
                out,
                "  .clone NEWDB              Copy the database to a new file."
            )
            .unwrap();
            writeln!(
                out,
                "  .restore FILE             Replace this database's content with FILE's."
            )
            .unwrap();
            writeln!(
                out,
                "  .open FILE                Close and reopen on a different database."
            )
            .unwrap();
            writeln!(
                out,
                "  .output [FILE]            Redirect output (no arg: back to stdout)."
            )
            .unwrap();
            writeln!(
                out,
                "  .once ?-e|-x|-w? ?FILE?    Redirect the NEXT statement's output."
            )
            .unwrap();
            writeln!(
                out,
                "      -e temp .txt + opener, -x (or .excel) temp CSV + spreadsheet,"
            )
            .unwrap();
            writeln!(
                out,
                "      -w (or .www) temp .html + browser (--plain: text page)"
            )
            .unwrap();
            writeln!(
                out,
                "  .excel                    Show the next query in a spreadsheet (.once -x)."
            )
            .unwrap();
            writeln!(
                out,
                "  .www ?--plain?            Show the next query in a browser (.once -w)."
            )
            .unwrap();
            writeln!(out, "  .mode list|html|json|table|csv|line Output mode.").unwrap();
            writeln!(
                out,
                "  .headers on|off           Column headers in table/csv output."
            )
            .unwrap();
            writeln!(
                out,
                "  .nullvalue STRING         Render SQL NULL as STRING (default: empty)."
            )
            .unwrap();
            writeln!(
                out,
                "  .width N ...              Fixed table-mode column widths (0 = auto)."
            )
            .unwrap();
            writeln!(
                out,
                "  .separator COL [ROW]      CSV separators (default: comma / newline)."
            )
            .unwrap();
            writeln!(out, "  .timer on|off             Per-statement wall time.").unwrap();
            writeln!(
                out,
                "  .changes on|off           Per-statement change counts."
            )
            .unwrap();
            writeln!(
                out,
                "  .echo on|off              Echo each statement before running it."
            )
            .unwrap();
            writeln!(
                out,
                "  .eqp on|off               EXPLAIN QUERY PLAN before each statement."
            )
            .unwrap();
            writeln!(out, "  .sha3sum ?--schema? ?--sha3-224|256|384|512? ?LIKE?").unwrap();
            writeln!(
                out,
                "  .archive ..ar ...         Manage SQL archives (zip/sqlar)."
            )
            .unwrap();
            writeln!(
                out,
                "                            SHA3 hash of the database content."
            )
            .unwrap();
            writeln!(
                out,
                "  .fullschema ?--indent?    Schema + sqlite_stat dumps."
            )
            .unwrap();
            writeln!(out, "  .log FILE|on|off          Redirect the log stream.").unwrap();
            writeln!(
                out,
                "  .scanstats on|off|est     Statement scan-status toggles."
            )
            .unwrap();
            writeln!(out, "  .print TEXT...            Print literal text.").unwrap();
            writeln!(out, "  .prompt MAIN [CONT]       Change the shell prompts.").unwrap();
            writeln!(
                out,
                "  .shell CMD / .system CMD  Run a shell command (local only)."
            )
            .unwrap();
            writeln!(out, "  .quit / .exit             Exit the shell.").unwrap();
        }
        ".tables" => {
            // sqlite3: the optional argument is a LIKE pattern.
            let sql = match parts.get(1) {
                Some(p) => format!(
                    "SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' AND name LIKE '{}' ORDER BY name",
                    p
                ),
                None => "SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name".to_string(),
            };
            let (_, rows) = runner.query(&sql)?;
            for row in rows {
                if let Some(Value::Text(n)) = row.first() {
                    writeln!(out, "{}", n.as_str()).unwrap();
                }
            }
        }
        ".indexes" => {
            let sql = match parts.get(1) {
                Some(p) => format!(
                    "SELECT name FROM sqlite_master WHERE type = 'index' AND name NOT LIKE 'sqlite_%' AND name LIKE '{}' ORDER BY name",
                    p
                ),
                None => "SELECT name FROM sqlite_master WHERE type = 'index' AND name NOT LIKE 'sqlite_%' ORDER BY name".to_string(),
            };
            let (_, rows) = runner.query(&sql)?;
            for row in rows {
                if let Some(Value::Text(n)) = row.first() {
                    writeln!(out, "{}", n.as_str()).unwrap();
                }
            }
        }
        ".databases" => {
            let (_, rows) = runner.query("PRAGMA database_list")?;
            for row in rows {
                let seq = row.first().map(|v| v.to_string()).unwrap_or_default();
                let name = row
                    .get(1)
                    .map(|v| match v {
                        Value::Text(t) => t.as_str().to_string(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                let file = match row.get(2) {
                    Some(Value::Text(t)) if !t.is_empty() => t.as_str().to_string(),
                    _ => String::new(),
                };
                writeln!(out, "{}: {} {}", seq, name, file).unwrap();
            }
        }
        ".dbinfo" => {
            for pragma in [
                "page_size",
                "page_count",
                "freelist_count",
                "schema_version",
                "journal_mode",
                "encoding",
                "auto_vacuum",
                "user_version",
            ] {
                let (_, rows) = runner.query(&format!("PRAGMA {}", pragma))?;
                let v = rows
                    .first()
                    .and_then(|r| r.first())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "NULL".to_string());
                writeln!(out, "{:<16} {}", format!("{}:", pragma), v).unwrap();
            }
        }
        ".stats" => {
            if let Some(arg) = parts.get(1) {
                let on = !arg.eq_ignore_ascii_case("off");
                if on {
                    print_shell_stats(runner, out)?;
                }
                // No persistent stats stream in rustqlite; the toggle
                // prints once (sqlite3 prints per-statement deltas).
                let _ = on;
                return Ok(());
            }
            print_shell_stats(runner, out)?;
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
                state.mode = match parts[1] {
                    "json" => OutputMode::Json,
                    "table" => OutputMode::Table,
                    "csv" => OutputMode::Csv,
                    "line" => OutputMode::Line,
                    "list" => OutputMode::List,
                    "html" => OutputMode::Html,
                    _ => {
                        writeln!(out, "unknown mode: {}", parts[1]).unwrap();
                        return Ok(());
                    }
                };
            } else {
                writeln!(out, "current mode: {:?}", state.mode).unwrap();
            }
        }
        ".headers" => match parts.get(1).map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("on") => state.headers = true,
            Some("off") => state.headers = false,
            _ => writeln!(out, "usage: .headers on|off").unwrap(),
        },
        ".nullvalue" => {
            state.nullvalue = parts
                .get(1)
                .map(|s| unquote_shell_word(s))
                .unwrap_or_default();
        }
        ".timer" => match parts.get(1).map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("on") => state.timer = true,
            Some("off") => state.timer = false,
            _ => writeln!(out, "usage: .timer on|off").unwrap(),
        },
        ".changes" => match parts.get(1).map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("on") => state.changes = true,
            Some("off") => state.changes = false,
            _ => writeln!(out, "usage: .changes on|off").unwrap(),
        },
        ".echo" => match parts.get(1).map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("on") => state.echo = true,
            Some("off") => state.echo = false,
            _ => writeln!(out, "usage: .echo on|off").unwrap(),
        },
        ".eqp" => match parts.get(1).map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("on") => state.eqp = true,
            Some("off") => state.eqp = false,
            _ => writeln!(out, "usage: .eqp on|off").unwrap(),
        },
        ".width" => {
            state.widths = parts[1..]
                .iter()
                .filter_map(|w| w.parse::<usize>().ok())
                .collect();
        }
        ".separator" => {
            if let Some(col) = parts.get(1) {
                state.col_sep = unquote_shell_word(col);
            }
            if let Some(row) = parts.get(2) {
                state.row_sep = unquote_shell_word(row);
            }
            if parts.len() == 1 {
                writeln!(out, "col: {}  row: {}", state.col_sep, state.row_sep).unwrap();
            }
        }
        ".output" => match parts.get(1) {
            Some(file) => {
                state.output_file = Some(file.to_string());
                // Status to stderr, like sqlite3's silence on success:
                // it must never pollute the redirected file or stdout.
                eprintln!("output redirected to {}", file);
            }
            None => {
                state.output_file = None;
                eprintln!("output restored to stdout");
            }
        },
        ".print" => {
            let text = parts[1..].join(" ");
            writeln!(out, "{}", text).unwrap();
        }
        ".prompt" => {
            if let Some(main) = parts.get(1) {
                state.prompt_main = main.to_string();
            }
            if let Some(cont) = parts.get(2) {
                state.prompt_cont = cont.to_string();
            }
        }
        ".save" | ".clone" => {
            let file = parts
                .get(1)
                .ok_or_else(|| format!("usage: {} FILE", parts[0]))?;
            let image = runner.backup_image()?;
            std::fs::write(file, &image).map_err(|e| format!("cannot write {}: {}", file, e))?;
            writeln!(out, "written: {} ({} bytes)", file, image.len()).unwrap();
        }
        ".open" => {
            let file = parts
                .get(1)
                .ok_or_else(|| "usage: .open FILE".to_string())?;
            if runner.is_remote() {
                return Err(".open is not available in remote mode".to_string());
            }
            let mut db = Database::open(file).map_err(|e| e.to_string())?;
            let _ = rustqlite::plugin::sha3::register(&mut db);
            let _ = rustqlite::plugin::filefns::register(&mut db);
            *runner = Runner::Local(Box::new(db));
            writeln!(out, "opened: {}", file).unwrap();
        }
        ".restore" => {
            let file = parts
                .get(1)
                .ok_or_else(|| "usage: .restore FILE".to_string())?;
            restore_from_file(runner, file, out)?;
        }
        ".shell" | ".system" => {
            let cmd = parts[1..].join(" ");
            if cmd.is_empty() {
                writeln!(out, "usage: {} COMMAND", parts[0]).unwrap();
                return Ok(());
            }
            run_shell_command(&cmd, out)?;
        }
        ".once" => {
            // `.once [OPTIONS] FILE` — sqlite3 3.53's option surface:
            // -e (text editor), -x (spreadsheet), -w (browser, with
            // --plain), -bom, or a plain FILE (options -e/-x/-w take NO
            // file — the temp path is generated).
            let mut file: Option<String> = None;
            let mut kind: Option<OnceKind> = None;
            let mut plain = false;
            let mut bom = false;
            let mut i = 1;
            while i < parts.len() {
                let arg = parts[i];
                if arg == "-e" || arg == "--e" {
                    kind = Some(OnceKind::Editor);
                } else if arg == "-x" || arg == "--x" {
                    kind = Some(OnceKind::Spreadsheet);
                } else if arg == "-w" || arg == "--w" {
                    kind = Some(OnceKind::Browser { plain: false });
                } else if arg == "--plain" {
                    plain = true;
                    if let Some(OnceKind::Browser { .. }) = kind {
                        kind = Some(OnceKind::Browser { plain: true });
                    }
                } else if arg == "-bom" || arg == "--bom" {
                    bom = true;
                } else if arg.starts_with('-') && arg.len() > 1 {
                    eprintln!("line 1: {} is not a valid option", arg);
                    return Ok(());
                } else if file.is_none() && kind.is_none() {
                    file = Some(arg.to_string());
                } else {
                    eprintln!("line 1: surplus argument");
                    return Ok(());
                }
                i += 1;
            }
            if let Some(kind) = kind {
                // -e/-x/-w: generated temp file under $HOME.
                if file.is_some() {
                    eprintln!("line 1: surplus argument");
                    return Ok(());
                }
                let kind = if let OnceKind::Browser { .. } = kind {
                    OnceKind::Browser { plain }
                } else {
                    kind
                };
                start_once_special(state, kind, bom, out)?;
            } else if let Some(f) = file {
                state.once_file = Some(f);
            } else {
                eprintln!("usage: .once ?OPTIONS? FILE");
            }
        }
        ".excel" => {
            // Shorthand for `.once -x`.
            if parts.len() > 1 {
                eprintln!("line 1: surplus argument");
                return Ok(());
            }
            start_once_special(state, OnceKind::Spreadsheet, cfg!(windows), out)?;
        }
        ".www" => {
            let plain = parts.contains(&"--plain");
            let extra: Vec<&str> = parts[1..]
                .iter()
                .filter(|a| **a != "--plain")
                .copied()
                .collect();
            if !extra.is_empty() {
                eprintln!("line 1: surplus argument");
                return Ok(());
            }
            start_once_special(state, OnceKind::Browser { plain }, false, out)?;
        }
        ".log" => {
            if parts.len() != 2 {
                eprintln!("Usage: .log FILENAME");
            } else {
                match parts[1] {
                    "off" => state.log_file = None,
                    "on" => {
                        // "on" redirects to stdout.
                        state.log_file = None; // stdout logging is the default
                        eprintln!("log output switched to stdout");
                    }
                    path => {
                        state.log_file = Some(
                            std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(path)
                                .map_err(|e| format!("cannot open log file {}: {}", path, e))?,
                        );
                    }
                }
            }
        }
        ".scanstats" => {
            if parts.len() == 2 {
                match parts[1] {
                    "on" | "est" | "vm" | "off" => {
                        state.scanstats = match parts[1] {
                            "on" => 1,
                            "est" => 2,
                            "vm" => 3,
                            _ => 0,
                        };
                        // Honest parity with a SQLite build without
                        // SQLITE_ENABLE_STMT_SCANSTATUS.
                        eprintln!("Warning: .scanstats not available in this build.");
                    }
                    _ => {
                        eprintln!("Usage: .scanstats on|off|est");
                    }
                }
            } else {
                eprintln!("Usage: .scanstats on|off|est");
            }
        }
        ".fullschema" => {
            if parts.len() == 2 && parts[1] == "--indent" {
                // Accepted (sqlite3's --indent reformats long CREATE
                // statements; schema text is emitted as stored).
                run_fullschema(runner, out)?;
            } else if parts.len() == 1 {
                run_fullschema(runner, out)?;
            } else {
                eprintln!("Usage: .fullschema ?--indent?");
            }
        }
        ".sha3sum" => run_sha3sum(runner, &parts[1..], state, out)?,
        ".archive" => {
            archive::run_archive(runner, &parts[1..], out)?;
        }
        ".quit" | ".exit" => {
            let _ = out.flush();
            std::process::exit(0);
        }
        _ => {
            writeln!(out, "unknown command: {} (try .help)", parts[0]).unwrap();
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// .once -e/-x/-w / .excel / .www helpers
// ---------------------------------------------------------------------------

/// Begin a `.once -e|-x|-w` capture: generate the temp path under
/// $HOME (sqlite3's `temp-<rand>.<ext>` naming), snapshot the shell
/// settings, and switch the next statement's output/mode.
fn start_once_special(
    state: &mut ShellState,
    kind: OnceKind,
    bom: bool,
    out: &mut ShellOut,
) -> Result<(), String> {
    let suffix = match kind {
        OnceKind::Editor => "txt",
        OnceKind::Spreadsheet => "csv",
        OnceKind::Browser { .. } => "html",
    };
    let path = new_temp_file(suffix);
    // sqlite3 writes the UTF-8 BOM for -x on Windows (Excel needs it);
    // an explicit -bom forces it everywhere.
    if bom {
        let _ = std::fs::write(&path, [0xEF, 0xBB, 0xBF]);
    }
    let saved = ModeSnapshot {
        mode: state.mode,
        col_sep: state.col_sep.clone(),
        row_sep: state.row_sep.clone(),
        headers: state.headers,
    };
    match kind {
        OnceKind::Editor => {
            // Text editor: mode unchanged, output to the temp .txt.
        }
        OnceKind::Spreadsheet => {
            state.mode = OutputMode::Csv;
            state.col_sep = ",".to_string();
            state.row_sep = "\r\n".to_string();
            state.csv_unquoted = true;
            // sqlite3's .excel always emits the BOM on Windows.
            if cfg!(windows) {
                let _ = std::fs::write(&path, [0xEF, 0xBB, 0xBF]);
            }
        }
        OnceKind::Browser { plain } => {
            if plain {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|e| format!("cannot open {}: {}", path, e))?;
                use std::io::Write;
                let _ = f.write_all(b"<!DOCTYPE html>\n<BODY>\n<PLAINTEXT>\n");
                state.mode = OutputMode::List;
            } else {
                state.mode = OutputMode::Html;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|e| format!("cannot open {}: {}", path, e))?;
                use std::io::Write;
                let _ = f.write_all(b"<!DOCTYPE html>\n<HTML><BODY><PRE>\n");
            }
        }
    }
    let _ = out; // status never pollutes the redirected stream
    state.once_special = Some(OnceSpecial { path, kind, saved });
    Ok(())
}

/// A `~/temp-<30 lowercase-alnum>.<suffix>` path, sqlite3's naming.
fn new_temp_file(suffix: &str) -> String {
    const ALPHA: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15)
        ^ (std::process::id() as u64).wrapping_mul(0x2545F4914F6CDD1D);
    let mut rand = String::with_capacity(30);
    for _ in 0..30 {
        // xorshift64* — cheap uniqueness is all that's needed.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        rand.push(ALPHA[(seed.wrapping_mul(0x2545F4914F6CDD1D) % 36) as usize] as char);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{}/temp-{}.{}", home.trim_end_matches('/'), rand, suffix)
}

/// The system opener sqlite3 uses (start / open / xdg-open).
fn open_with_system_opener(path: &str) -> bool {
    let prog = if cfg!(windows) {
        "start"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::io::Write::flush(&mut std::io::stderr());
    match std::process::Command::new(prog)
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(st) => st.success(),
        Err(_) => {
            eprintln!("Failed: [{} {}]", prog, path);
            false
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite's LIKE (sqlite3_strlike) for .sha3sum's table filter
// ---------------------------------------------------------------------------

/// Case-insensitive (ASCII) LIKE with `%` / `_` and an escape char
/// (`esc == 0` disables escaping). sqlite3_strlike semantics.
fn strlike(pattern: &str, text: &str, esc: u8) -> bool {
    fn match_from(p: &[u8], t: &[u8], esc: u8) -> bool {
        let (mut pi, mut ti) = (0usize, 0usize);
        while pi < p.len() {
            let pc = p[pi];
            if esc != 0 && pc == esc && pi + 1 < p.len() {
                // Escaped literal char.
                if ti >= t.len() || !p[pi + 1].eq_ignore_ascii_case(&t[ti]) {
                    return false;
                }
                pi += 2;
                ti += 1;
            } else if pc == b'%' {
                // Try all splits (greedy backtracking).
                let rest = &p[pi + 1..];
                if rest.is_empty() {
                    return true;
                }
                for k in ti..=t.len() {
                    if match_from(rest, &t[k..], esc) {
                        return true;
                    }
                }
                return false;
            } else if pc == b'_' {
                if ti >= t.len() {
                    return false;
                }
                pi += 1;
                ti += 1;
            } else {
                if ti >= t.len() || !pc.eq_ignore_ascii_case(&t[ti]) {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
        ti == t.len()
    }
    match_from(pattern.as_bytes(), text.as_bytes(), esc)
}

// ---------------------------------------------------------------------------
// .sha3sum — sqlite3's shathree.c-backed command, byte-identical
// ---------------------------------------------------------------------------

fn run_sha3sum(
    runner: &mut Runner,
    args: &[&str],
    state: &mut ShellState,
    out: &mut ShellOut,
) -> Result<(), String> {
    use rustqlite::plugin::sha3::{sha3_update_stmt, sha3_update_value, Sha3};

    let mut b_schema = false;
    let mut b_separate = false;
    let mut size: u32 = 224;
    let mut b_debug = false;
    let mut z_like: Option<String> = None;
    for arg in args {
        if let Some(stripped) = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-')) {
            match stripped {
                "schema" => b_schema = true,
                "sha3-224" | "sha3-256" | "sha3-384" | "sha3-512" => {
                    size = stripped[5..].parse().unwrap_or(224);
                }
                "debug" => b_debug = true,
                _ => {
                    eprintln!("Unknown option \"{}\" on \".sha3sum\"", arg);
                    eprintln!("Usage: .sha3sum ?OPTIONS? ?LIKE-PATTERN?");
                    return Ok(());
                }
            }
        } else if z_like.is_some() {
            eprintln!("Usage: .sha3sum ?OPTIONS? ?LIKE-PATTERN?");
            return Ok(());
        } else {
            z_like = Some((*arg).to_string());
            b_separate = true;
            // A pattern matching `sqlite_*` flips schema mode on.
            if strlike("sqlite\\_%", arg, b'\\') {
                b_schema = true;
            }
        }
    }

    // The table list (lowercased names, sorted — sqlite3's `ORDER BY 1
    // COLLATE NOCASE` on already-lowercased names is a byte sort).
    let (cols, rows) = runner
        .query(
            "SELECT lower(name), rootpage FROM sqlite_schema WHERE type='table' \
             AND coalesce(rootpage,0)>1",
        )
        .map_err(|e| format!(".sha3sum failed: {}", e))?;
    let _ = cols;
    let mut tables: Vec<String> = rows
        .iter()
        .filter_map(|r| {
            let name = r.first()?.as_text();
            if !b_schema && name.starts_with("sqlite_") {
                return None;
            }
            Some(name)
        })
        .collect();
    if b_schema {
        tables.push("sqlite_schema".to_string());
    }
    tables.sort();
    tables.dedup();
    // Apply the LIKE pattern (on the lowercased name).
    if let Some(pat) = z_like.as_deref() {
        tables.retain(|t| strlike(pat, t, 0));
    }

    // Build the per-table query strings EXACTLY as sqlite3's .sha3sum:
    // `SELECT * FROM "<name>" NOT INDEXED;` plus the four internal-table
    // forms (sqlite_stat4's carries a trailing newline — which in the
    // combined hash becomes the NEXT statement's leading byte, exactly
    // like concatenating the statements and preparing them in sequence).
    let mut queries: Vec<(String, String)> = Vec::new(); // (query, table)
    for t in &tables {
        let q = match t.as_str() {
            "sqlite_schema" => {
                "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY name;".to_string()
            }
            "sqlite_sequence" => "SELECT name,seq FROM sqlite_sequence ORDER BY name;".to_string(),
            "sqlite_stat1" => "SELECT tbl,idx,stat FROM sqlite_stat1 ORDER BY tbl,idx;".to_string(),
            "sqlite_stat4" => "SELECT * FROM sqlite_stat4 ORDER BY tbl, idx, rowid;\n".to_string(),
            other if other.starts_with("sqlite_") => String::new(),
            other => format!(
                "SELECT * FROM \"{}\" NOT INDEXED;",
                other.replace('"', "\"\"")
            ),
        };
        queries.push((q, t.clone()));
    }

    if b_debug {
        // The exact WITH query sqlite3 builds (for --debug parity).
        let mut s = String::from("WITH [sha3sum$query](a,b) AS(");
        let mut sep = "VALUES(";
        for (q, t) in &queries {
            s.push_str(sep);
            s.push('\'');
            s.push_str(&q.replace('\'', "''"));
            s.push('\'');
            s.push(',');
            s.push('\'');
            s.push_str(t);
            s.push('\'');
            sep = "),(";
        }
        s.push_str("))");
        if b_separate {
            s.push_str(&format!(
                " SELECT lower(hex(sha3_query(a,{}))) AS hash, b AS label   FROM [sha3sum$query]",
                size
            ));
        } else {
            s.push_str(&format!(
                " SELECT lower(hex(sha3_query(group_concat(a,''),{}))) AS hash   FROM [sha3sum$query]",
                size
            ));
        }
        let _ = writeln!(out, "{}", s);
    }

    // Hash. Combined mode: one context over the statement sequence
    // (stat4's trailing '\n' prefixes the following statement's text,
    // like sqlite3_prepare_v2 over the concatenation).
    let hash_one = |qs: &[(String, String)]| -> Result<String, String> {
        let mut h = Sha3::new(size).map_err(|e| e.to_string())?;
        let mut carry = String::new();
        for (q, _) in qs {
            let effective = format!("{}{}", carry, q);
            // The statement runs up to the first ';'.
            let exec = match effective.find(';') {
                Some(pos) => effective[..=pos].to_string(),
                None => effective.clone(),
            };
            if exec.trim().is_empty() && effective.is_empty() {
                carry = String::new();
                continue;
            }
            sha3_update_stmt(&mut h, &exec);
            let (_cols, rows) = runner.query(&exec).map_err(|e| e.to_string())?;
            for row in &rows {
                h.update(b"R");
                for v in row {
                    sha3_update_value(&mut h, v);
                }
            }
            // Bytes after the final ';' carry into the next statement.
            carry = match effective.rfind(';') {
                Some(pos) => effective[pos + 1..].to_string(),
                None => String::new(),
            };
        }
        let digest = h.finalize();
        Ok(digest.iter().map(|b| format!("{:02x}", b)).collect())
    };

    let out_cols: Vec<String> = if b_separate {
        vec!["hash".to_string(), "label".to_string()]
    } else {
        vec!["hash".to_string()]
    };
    let out_rows: Vec<Vec<Value>> = if b_separate {
        let mut acc = Vec::new();
        for (q, t) in &queries {
            let entry = (q.clone(), t.clone());
            let hash = hash_one(std::slice::from_ref(&entry))?;
            acc.push(vec![
                Value::Text(hash.into()),
                Value::Text(t.clone().into()),
            ]);
        }
        acc
    } else if queries.is_empty() {
        // sqlite3's empty CTE (AS() with no VALUES rows) yields NO
        // output — only the trailing ".sha3sum failed."
        Vec::new()
    } else {
        let hash = hash_one(&queries)?;
        vec![vec![Value::Text(hash.into())]]
    };

    if !out_rows.is_empty() {
        print_rows(out, &out_cols, &out_rows, state);
    }

    // sqlite3's reversible-text check runs on the USER tables (always
    // excluding sqlite_*): an empty set makes the generated check SQL
    // invalid — the ".sha3sum failed." tail (rc=1). rustqlite's TEXT is
    // always valid UTF-8, so the count itself is always 0; only the
    // empty-set quirk is observable.
    let has_user_tables = tables.iter().any(|t| !t.starts_with("sqlite_"));
    if !has_user_tables {
        eprintln!(".sha3sum failed.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// .fullschema — schema + sqlite_stat dumps (sqlite3's ANALYZE sandwich)
// ---------------------------------------------------------------------------

fn run_fullschema(runner: &mut Runner, out: &mut ShellOut) -> Result<(), String> {
    // Schema objects in rowid order, `sql;` each — internal (sqlite_*)
    // objects are filtered out, like sqlite3's
    // `name NOT LIKE 'sqlite__%' ESCAPE '_'`.
    let (_, rows) = runner
        .query(
            "SELECT name, sql FROM sqlite_schema WHERE sql IS NOT NULL \
             ORDER BY rowid",
        )
        .map_err(|e| e.to_string())?;
    for r in &rows {
        let name = r.first().map(|v| v.as_text()).unwrap_or_default();
        if name.starts_with("sqlite_") {
            continue;
        }
        let sql = r.get(1).map(|v| v.as_text()).unwrap_or_default();
        let _ = writeln!(out, "{};", sql);
    }

    // Which stat tables exist?
    let (_, srows) = runner
        .query(
            "SELECT name FROM sqlite_schema WHERE type='table' \
             AND name IN ('sqlite_stat1','sqlite_stat4')",
        )
        .map_err(|e| e.to_string())?;
    let names: Vec<String> = srows
        .iter()
        .filter_map(|r| r.first().map(|v| v.as_text()))
        .collect();
    if names.is_empty() {
        let _ = writeln!(out, "/* No STAT tables available */");
    } else {
        let _ = writeln!(out, "ANALYZE sqlite_schema;");
        for tbl in ["sqlite_stat1", "sqlite_stat4"] {
            if !names.iter().any(|n| n == tbl) {
                continue;
            }
            let (_, rows) = runner
                .query(&format!("SELECT * FROM {}", tbl))
                .map_err(|e| e.to_string())?;
            for r in &rows {
                let rendered: Vec<String> = r.iter().map(insert_literal).collect();
                let _ = writeln!(out, "INSERT INTO {} VALUES({});", tbl, rendered.join(","));
            }
        }
        let _ = writeln!(out, "ANALYZE sqlite_schema;");
    }
    Ok(())
}

/// sqlite3's `.mode insert` literal rendering.
fn insert_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => rustqlite::types::format_real(*f),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => {
            let hex: String = b.iter().map(|x| format!("{:02X}", x)).collect();
            format!("X'{}'", hex)
        }
    }
}

/// Strip one level of matching quotes from a shell word ('...' / "...").
fn unquote_shell_word(w: &str) -> String {
    let bytes = w.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return w[1..w.len() - 1].to_string();
        }
    }
    w.to_string()
}

/// `.stats`: page + schema statistics (sqlite3's page-stats subset).
fn print_shell_stats(runner: &mut Runner, out: &mut ShellOut) -> Result<(), String> {
    let page_size: i64 = runner
        .query("PRAGMA page_size")?
        .1
        .first()
        .and_then(|r| r.first())
        .and_then(|v| match v {
            Value::Integer(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(0);
    let page_count: i64 = runner
        .query("PRAGMA page_count")?
        .1
        .first()
        .and_then(|r| r.first())
        .and_then(|v| match v {
            Value::Integer(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(0);
    let freelist: i64 = runner
        .query("PRAGMA freelist_count")?
        .1
        .first()
        .and_then(|r| r.first())
        .and_then(|v| match v {
            Value::Integer(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(0);
    writeln!(out, "page size:        {}", page_size).unwrap();
    writeln!(out, "page count:       {}", page_count).unwrap();
    writeln!(out, "database size:    {} bytes", page_size * page_count).unwrap();
    writeln!(out, "freelist pages:   {}", freelist).unwrap();
    let (_, objects) =
        runner.query("SELECT type, count(*) FROM sqlite_master GROUP BY type ORDER BY type")?;
    for row in &objects {
        let kind = row
            .first()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string());
        let n = row.get(1).map(|v| v.to_string()).unwrap_or_default();
        writeln!(out, "schema {}:   {}", kind, n).unwrap();
    }
    Ok(())
}

/// `.restore FILE`: replace this database's content with FILE's —
/// driven entirely through ATTACH (the round's new engine surface):
/// every schema object is recreated from the source's `sqlite_master`
/// and its rows copied table by table, inside one transaction.
fn restore_from_file(runner: &mut Runner, file: &str, out: &mut ShellOut) -> Result<(), String> {
    if !std::path::Path::new(file).exists() {
        return Err(format!("cannot read {}: no such file", file));
    }
    let quoted = file.replace(char::from(39), "''");
    runner.execute(&format!("ATTACH '{}' AS __restore_src", quoted))?;
    let result = (|| -> Result<(), String> {
        // Drop current user objects (schema order preserved on rebuild).
        let (_, current) = runner.query("SELECT type, name FROM sqlite_master ORDER BY rowid")?;
        for row in current {
            let (kind, name) = match (row.first(), row.get(1)) {
                (Some(Value::Text(a)), Some(Value::Text(b))) => {
                    (a.as_str().to_string(), b.as_str().to_string())
                }
                _ => continue,
            };
            if name.starts_with("sqlite_") {
                continue;
            }
            let drop_sql = match kind.as_str() {
                "table" => format!("DROP TABLE IF EXISTS main.\"{}\"", name),
                "view" => format!("DROP VIEW IF EXISTS main.\"{}\"", name),
                "index" => format!("DROP INDEX IF EXISTS main.\"{}\"", name),
                "trigger" => format!("DROP TRIGGER IF EXISTS main.\"{}\"", name),
                _ => continue,
            };
            runner.execute(&drop_sql)?;
        }
        // Recreate every object from the source, in creation order.
        let (_, schema) = runner.query(
            "SELECT type, name, sql FROM __restore_src.sqlite_master \
             WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid",
        )?;
        // Tables first (FKs and triggers reference them), then the rest.
        for pass in 0..2 {
            for row in &schema {
                let kind = match row.first() {
                    Some(Value::Text(t)) => t.as_str().to_string(),
                    _ => continue,
                };
                let is_table = kind == "table";
                if (pass == 0) != is_table {
                    continue;
                }
                let sql = match row.get(2) {
                    Some(Value::Text(t)) => t.as_str().to_string(),
                    _ => continue,
                };
                runner.execute(&sql)?;
            }
        }
        // Copy rows table by table.
        let (_, tables) = runner.query(
            "SELECT name FROM __restore_src.sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY rowid",
        )?;
        let mut copied = 0usize;
        for row in &tables {
            let name = match row.first() {
                Some(Value::Text(t)) => t.as_str().to_string(),
                _ => continue,
            };
            runner.execute(&format!(
                "INSERT INTO main.\"{}\" SELECT * FROM __restore_src.\"{}\"",
                name, name
            ))?;
            copied += 1;
        }
        writeln!(out, "restored: {} tables from {}", copied, file).unwrap();
        Ok(())
    })();
    let detach = runner.execute("DETACH __restore_src");
    result?;
    detach.map_err(|e| e.to_string())?;
    Ok(())
}

/// `.shell` / `.system`: run a command through the platform shell.
fn run_shell_command(cmd: &str, out: &mut ShellOut) -> Result<(), String> {
    use std::process::Command;
    let output = if cfg!(windows) {
        Command::new("cmd").args(["/C", cmd]).output()
    } else {
        Command::new("sh").args(["-c", cmd]).output()
    }
    .map_err(|e| format!("cannot run shell: {}", e))?;
    write!(out, "{}", String::from_utf8_lossy(&output.stdout)).unwrap();
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
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
        BatchOp::ExportSqlite(file) => {
            runner.export_sqlite_image(file)?;
            println!("sqlite-format export written: {}", file);
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
    state: &ShellState,
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

    if state.echo {
        writeln!(out, "{}", sql).unwrap();
    }
    let started = std::time::Instant::now();
    if is_query {
        if state.eqp {
            // sqlite3's .eqp: the plan renders BEFORE the results.
            if let Ok((pcols, prows)) = runner.query(&format!("EXPLAIN QUERY PLAN {}", sql)) {
                print_rows(out, &pcols, &prows, state);
            }
        }
        let (cols, rows) = runner.query(sql)?;
        print_rows(out, &cols, &rows, state);
    } else {
        runner.execute(sql)?;
        if state.changes {
            let (chg, total) = (runner.changes(), runner.total_changes());
            writeln!(out, "changes: {}   total_changes: {}", chg, total).unwrap();
        } else {
            writeln!(out, "OK").unwrap();
        }
    }
    if state.timer {
        writeln!(
            out,
            "Run Time (s): real {:.3}",
            started.elapsed().as_secs_f64()
        )
        .unwrap();
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum OutputMode {
    Table,
    Json,
    Csv,
    Line,
    /// sqlite3's default: raw values joined by `|` (no quoting).
    List,
    /// sqlite3's `.mode html`: <TR>/<TH>/<TD> rows (no closing tags).
    Html,
}

fn print_rows(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>], state: &ShellState) {
    if rows.is_empty() {
        writeln!(out, "(no rows)").unwrap();
        return;
    }
    match state.mode {
        OutputMode::Table => print_table(out, cols, rows, state),
        OutputMode::Json => print_json(out, cols, rows),
        OutputMode::Csv => print_csv(out, cols, rows, state),
        OutputMode::Line => print_line(out, cols, rows, state),
        OutputMode::List => print_list(out, cols, rows, state),
        OutputMode::Html => print_html(out, cols, rows, state),
    }
}

fn cell_text(v: &Value, state: &ShellState) -> String {
    match v {
        Value::Null => state.nullvalue.clone(),
        _ => format!("{}", v),
    }
}

fn print_table(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>], state: &ShellState) {
    // Column widths: `.width` overrides (0 = auto), else content-driven.
    let mut widths: Vec<usize> = cols.iter().map(|c| c.len()).collect();
    for row in rows {
        for (i, v) in row.iter().enumerate() {
            let len = cell_text(v, state).len();
            if i < widths.len() && len > widths[i] {
                widths[i] = len;
            }
        }
    }
    for (i, w) in state.widths.iter().enumerate() {
        if i < widths.len() && *w > 0 {
            widths[i] = *w;
        }
    }
    // Header
    if state.headers {
        let header: Vec<String> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:width$}", c, width = widths[i]))
            .collect();
        writeln!(out, "| {} |", header.join(" | ")).unwrap();
        // Separator
        let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        writeln!(out, "|-{}-|", sep.join("-|-")).unwrap();
    }
    // Rows
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let s = cell_text(v, state);
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

fn print_csv(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>], state: &ShellState) {
    if state.headers {
        let header: Vec<String> = cols
            .iter()
            .map(|c| c.replace(state.col_sep.as_str(), " "))
            .collect();
        write_line(out, &header.join(state.col_sep.as_str()), &state.row_sep);
    }
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| {
                let t = cell_text(v, state);
                // `.once -x` (the excel capture) writes UNQUOTED csv —
                // sqlite3's own quirk (.mode csv quotes, .excel does not).
                if !state.csv_unquoted && t.contains(state.col_sep.as_str()) {
                    format!("\"{}\"", t.replace('"', "\"\""))
                } else {
                    t
                }
            })
            .collect();
        write_line(out, &cells.join(state.col_sep.as_str()), &state.row_sep);
    }
}

fn print_line(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>], state: &ShellState) {
    let max_col_len = cols.iter().map(|c| c.len()).max().unwrap_or(0);
    for row in rows {
        for (i, v) in row.iter().enumerate() {
            let name = cols.get(i).map(|s| s.as_str()).unwrap_or("?");
            writeln!(
                out,
                "{:>width$} = {}",
                name,
                cell_text(v, state),
                width = max_col_len
            )
            .unwrap();
        }
        writeln!(out).unwrap();
    }
}

/// sqlite3's `.mode list`: raw cell text joined by `|` — NO quoting even
/// when values contain the separator (oracle-pinned).
fn print_list(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>], state: &ShellState) {
    let sep = "|";
    if state.headers {
        let line = cols.join(sep);
        let line = if line.is_empty() { String::new() } else { line };
        write_line(out, &line, "\n");
    }
    for row in rows {
        let cells: Vec<String> = row.iter().map(|v| cell_text(v, state)).collect();
        write_line(out, &cells.join(sep), "\n");
    }
}

/// Emit one line with the given row separator (handles the CRLF case).
fn write_line(out: &mut impl Write, line: &str, row_sep: &str) {
    if row_sep == "\n" {
        writeln!(out, "{}", line).unwrap();
    } else {
        write!(out, "{}{}", line, row_sep).unwrap();
    }
}

/// sqlite3's `.mode html`: each cell on its own line inside <TR> rows;
/// headers use <TH>, NULL renders `null` (oracle-pinned, no closing
/// tags; `&`, `<`, `>` escaped).
fn print_html(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>], state: &ShellState) {
    if state.headers {
        writeln!(out, "<TR>").unwrap();
        for c in cols {
            writeln!(out, "<TH>{}", html_escape(c)).unwrap();
        }
        writeln!(out, "</TR>").unwrap();
    }
    for row in rows {
        writeln!(out, "<TR>").unwrap();
        for v in row {
            let t = match v {
                Value::Null => "null".to_string(),
                other => cell_text(other, state),
            };
            writeln!(out, "<TD>{}", html_escape(&t)).unwrap();
        }
        writeln!(out, "</TR>").unwrap();
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
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
