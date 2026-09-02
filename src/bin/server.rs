//! HTTP/JSON server for rustqlite.
//!
//! Exposes a simple REST API for executing SQL queries against a database.
//!
//! ## Endpoints
//!
//! POST /query
//!   Body: {"sql": "SELECT ...", "params": [...]}
//!   Returns: {"columns": [...], "rows": [[...], ...]}
//!
//! POST /execute
//!   Body: {"sql": "INSERT ...", "params": [...]}
//!   Returns: {"ok": true} or {"error": "..."}
//!
//! GET /health
//!   Returns: {"status": "ok"}
//!
//! ## Concurrency model
//!
//! The database is wrapped in `Arc<RwLock<Database>>`. The TCP accept loop
//! is multi-threaded (N worker threads pull from the listener in parallel),
//! and request parsing happens in parallel across cores.
//!
//! **Reads (`/query`)** take a READ lock and call `Database::query_shared()`
//! (a `&self` method). Multiple readers run concurrently — no writer-head
//! contention, no serial lock across readers.
//!
//! **Writes (`/execute`)** take a WRITE lock and call `Database::execute()`
//! (a `&mut self` method). Writers are serialized, but a writer doesn't
//! block concurrent readers (it just queues for the next available write
//! window — readers proceed).
//!
//! This is enabled by the interior-mutability refactor on `Pager` (cache is
//! `RwLock<HashMap>`, page size / n_pages / freelist are `AtomicU32`, file
//! I/O uses positioned `pread`/`pwrite` so threads don't share an offset)
//! and on `Database` (`stmt_cache`/`root_overrides`/`max_rowids` are `RwLock`).
//!
//! ## Usage
//!
//!   rustqlite-server --db /path/to/db.sqlite --port 8080 --threads 8

use parking_lot::RwLock;
use rustqlite::{Database, Value};
use std::env;
use std::sync::Arc;
use std::thread;
use tiny_http::{Header, Method, Response, Server};

struct State {
    /// RwLock: read-locked for `/query` (concurrent reads), write-locked
    /// for `/execute` (serialized writes).
    db: RwLock<Database>,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut db_path = ":memory:".to_string();
    let mut port: u16 = 8080;
    let mut host = "127.0.0.1".to_string();
    let mut n_threads: usize = 4;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--db" | "-d" => {
                i += 1;
                if i < args.len() {
                    db_path = args[i].clone();
                }
            }
            "--port" | "-p" => {
                i += 1;
                if i < args.len() {
                    port = args[i].parse().unwrap_or(8080);
                }
            }
            "--host" | "-H" => {
                i += 1;
                if i < args.len() {
                    host = args[i].clone();
                }
            }
            "--threads" | "-t" => {
                i += 1;
                if i < args.len() {
                    n_threads = args[i].parse().unwrap_or(4);
                }
            }
            "--help" | "-h" => {
                println!(
                    "Usage: rustqlite-server [--db PATH] [--port PORT] [--host HOST] [--threads N]"
                );
                println!("  --db PATH    Database file path (default: in-memory)");
                println!("  --port PORT  TCP port (default: 8080)");
                println!("  --host HOST  Bind address (default: 127.0.0.1)");
                println!("  --threads N  Worker thread count (default: 4)");
                return;
            }
            _ => {
                eprintln!("unknown argument: {}", args[i]);
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let db = if db_path == ":memory:" {
        Database::open_in_memory().unwrap_or_else(|e| {
            eprintln!("error opening in-memory database: {}", e);
            std::process::exit(1);
        })
    } else {
        Database::open(&db_path).unwrap_or_else(|e| {
            eprintln!("error opening {}: {}", db_path, e);
            std::process::exit(1);
        })
    };

    let state = Arc::new(State {
        db: RwLock::new(db),
    });
    let addr = format!("{}:{}", host, port);
    let server = Arc::new(Server::http(&addr).unwrap_or_else(|e| {
        eprintln!("error binding to {}: {}", addr, e);
        std::process::exit(1);
    }));
    println!("rustqlite-server listening on http://{}", addr);
    println!("Database: {}", db_path);
    println!("Threads:  {}", n_threads);

    // Multi-threaded: spawn N worker threads, each pulling requests from the
    // shared server. tiny_http's `incoming_requests()` is thread-safe and
    // distributes connections across consumers.
    let mut handles = Vec::new();
    for _ in 0..n_threads {
        let state = Arc::clone(&state);
        let server = Arc::clone(&server);
        handles.push(thread::spawn(move || {
            for request in server.incoming_requests() {
                handle_request(request, &state);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}

fn handle_request(request: tiny_http::Request, state: &State) {
    let url = request.url().to_string();
    let method = request.method().clone();

    match (&method, url.as_str()) {
        (Method::Get, "/health") => {
            let _ = request.respond(
                Response::from_string(r#"{"status":"ok"}"#).with_header(content_type_json()),
            );
        }
        (Method::Post, "/query") => {
            handle_query(request, state);
        }
        (Method::Post, "/execute") => {
            handle_execute(request, state);
        }
        _ => {
            let _ = request.respond(
                Response::from_string(r#"{"error":"not found"}"#)
                    .with_status_code(404)
                    .with_header(content_type_json()),
            );
        }
    }
}

fn handle_query(mut request: tiny_http::Request, state: &State) {
    let body = match read_body(&mut request) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error reading body: {}", e);
            return;
        }
    };
    let parsed: ParsedRequest = match parse_request(&body) {
        Ok(p) => p,
        Err(e) => {
            respond_error(request, &e);
            return;
        }
    };

    // READ lock — multiple readers can run concurrently. This is the key
    // change that enables true concurrency: before the interior-mutability
    // refactor on `Pager` and `Database`, we had to take the write lock
    // here because `query()` required `&mut self`. Now `query_shared()`
    // takes `&self` and uses interior mutability for cache fills.
    let guard = state.db.read();
    match guard.query_with_columns(&parsed.sql, parsed.params) {
        Ok((cols, rows)) => {
            let json = format_query_result(&cols, &rows);
            let _ = request.respond(Response::from_string(json).with_header(content_type_json()));
        }
        Err(e) => {
            respond_error(request, &e.to_string());
        }
    }
}

fn handle_execute(mut request: tiny_http::Request, state: &State) {
    let body = match read_body(&mut request) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error reading body: {}", e);
            return;
        }
    };
    let parsed: ParsedRequest = match parse_request(&body) {
        Ok(p) => p,
        Err(e) => {
            respond_error(request, &e);
            return;
        }
    };

    let mut guard = state.db.write();
    match guard.execute(&parsed.sql, parsed.params) {
        Ok(_) => {
            let _ = request
                .respond(Response::from_string(r#"{"ok":true}"#).with_header(content_type_json()));
        }
        Err(e) => {
            respond_error(request, &e.to_string());
        }
    }
}

fn read_body(request: &mut tiny_http::Request) -> Result<String, String> {
    let mut body = String::new();
    request
        .as_reader()
        .read_to_string(&mut body)
        .map_err(|e| e.to_string())?;
    Ok(body)
}

struct ParsedRequest {
    sql: String,
    params: Vec<Value>,
}

fn parse_request(body: &str) -> Result<ParsedRequest, String> {
    let sql = extract_json_string(body, "sql").ok_or_else(|| "missing 'sql' field".to_string())?;
    let params = extract_json_array(body, "params").unwrap_or_default();
    Ok(ParsedRequest { sql, params })
}

fn extract_json_string(body: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let pos = body.find(&pattern)?;
    let rest = &body[pos + pattern.len()..];
    let colon = rest.find(':')?;
    let rest = &rest[colon + 1..];
    let rest = rest.trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let rest = &rest[1..];
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '"' {
            return Some(out);
        }
        if c == '\\' {
            if let Some(next) = chars.next() {
                match next {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    other => out.push(other),
                }
            }
        } else {
            out.push(c);
        }
    }
    None
}

fn extract_json_array(body: &str, key: &str) -> Option<Vec<Value>> {
    let pattern = format!("\"{}\"", key);
    let pos = body.find(&pattern)?;
    let rest = &body[pos + pattern.len()..];
    let colon = rest.find(':')?;
    let rest = &rest[colon + 1..];
    let rest = rest.trim_start();
    if !rest.starts_with('[') {
        return None;
    }
    let mut depth = 0;
    let mut end = 0;
    for (i, c) in rest.chars().enumerate() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    let array_str = &rest[1..end];
    let mut params = Vec::new();
    for item in array_str.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if item == "null" {
            params.push(Value::Null);
        } else if item.starts_with('"') && item.ends_with('"') {
            params.push(Value::Text(item[1..item.len() - 1].to_string().into()));
        } else if item.contains('.') {
            params.push(Value::Real(item.parse().unwrap_or(0.0)));
        } else {
            params.push(Value::Integer(item.parse().unwrap_or(0)));
        }
    }
    Some(params)
}

fn format_query_result(cols: &[String], rows: &[Vec<Value>]) -> String {
    let mut out = String::new();
    out.push_str("{\"columns\":[");
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{:?}", c));
    }
    out.push_str("],\"rows\":[");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (j, v) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&json_value(v));
        }
        out.push(']');
    }
    out.push_str("]}");
    out
}

fn json_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => format!("{}", f),
        Value::Text(s) => format!("{:?}", s),
        Value::Blob(b) => {
            let hex: String = b.iter().map(|x| format!("{:02x}", x)).collect();
            format!("\"{}\"", hex)
        }
    }
}

fn respond_error(request: tiny_http::Request, msg: &str) {
    let body = format!("{{\"error\":{:?}}}", msg);
    let _ = request.respond(
        Response::from_string(body)
            .with_status_code(400)
            .with_header(content_type_json()),
    );
}

fn content_type_json() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
}
