//! # rustqlite-compat — the SQLite C ABI, backed by rustqlite
//!
//! This crate exports the **real `sqlite3_*` C symbols** (`sqlite3_open_v2`,
//! `sqlite3_prepare_v3`, `sqlite3_bind_*`, `sqlite3_step`, `sqlite3_column_*`,
//! …) implemented on top of the rustqlite engine. The output artifact is a
//! drop-in `libsqlite3.so` / `libsqlite3.a`: any program, driver or ORM
//! written against the SQLite C API — including **unmodified sqlx and
//! sea-orm** — links against this library and runs on rustqlite instead of
//! SQLite.
//!
//! # Integration with sqlx / sea-orm (unmodified)
//!
//! ```toml
//! # Your app's Cargo.toml
//! [dependencies]
//! sqlx = { version = "0.9", default-features = false,
//!          features = ["runtime-tokio", "sqlite-unbundled", "macros", "migrate"] }
//! sea-orm = { version = "2.0", features = ["sqlx-sqlite", "runtime-tokio"] }
//!
//! [patch.crates-io]
//! libsqlite3-sys = { path = "path/to/rust-sql/compat/libsqlite3-sys" }
//! ```
//!
//! Then build once (`cargo build --release -p rustqlite-compat` in the
//! rust-sql repo) and the patched sys crate links + rpaths your binary to
//! `rust-sql/target/release/libsqlite3.so`. See `compat/README.md`.
//!
//! # Threading / connection model
//!
//! Every `sqlite3_open*` of the same file path in this process shares ONE
//! rustqlite engine instance (one pager, one page cache). SQL connections
//! therefore see each other's committed state immediately — no stale-cache
//! corruption, which is what a naive per-connection engine would give you.
//! Transactions are serialized across connections with SQLite-compatible
//! BUSY/busy_timeout semantics; reads run concurrently (the engine's
//! parallel-reader path). Isolation note (shared-cache-like): uncommitted
//! writes made inside an open transaction on connection A are visible to
//! reads on connection B until A commits or rolls back.
//!
//! `:memory:` opens a fresh private engine per connection (SQLite
//! semantics); `file:NAME?mode=memory&cache=shared` shares one named
//! in-memory engine across connections.
//!
//! # Semantics notes
//!
//! - Result codes are SQLite's, including extended codes
//!   (`SQLITE_CONSTRAINT_UNIQUE` = 2067 etc.).
//! - `sqlite3_column_count` / `column_name` work at PREPARE time (sqlx
//!   reads them before the first step). SELECT statements that the engine
//!   can't name without executing are executed once at prepare and reset
//!   (side-effect free — SELECTs only).
//! - DDL, transactions, SAVEPOINT and write-form PRAGMAs are preparable
//!   and stepped like in SQLite (executed on the first `sqlite3_step`).
//! - `PRAGMA foreign_keys` (sqlx sends `= ON` at connect) is honored.

#![allow(clippy::missing_safety_doc)]
#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uchar, c_void, CStr, CString};
use std::os::raw::{c_double, c_uchar as u_uchar};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use rustqlite::types::Value;
use rustqlite::{Database, Statement as EngineStatement, StepResult};

// ---------------------------------------------------------------------------
// SQLite status / constant codes (exact numeric values from sqlite3.h)
// ---------------------------------------------------------------------------

pub const SQLITE_OK: c_int = 0;
pub const SQLITE_ERROR: c_int = 1;
pub const SQLITE_INTERNAL: c_int = 2;
pub const SQLITE_PERM: c_int = 3;
pub const SQLITE_ABORT: c_int = 4;
pub const SQLITE_BUSY: c_int = 5;
pub const SQLITE_LOCKED: c_int = 6;
pub const SQLITE_NOMEM: c_int = 7;
pub const SQLITE_READONLY: c_int = 8;
pub const SQLITE_INTERRUPT: c_int = 9;
pub const SQLITE_IOERR: c_int = 10;
pub const SQLITE_CORRUPT: c_int = 11;
pub const SQLITE_NOTFOUND: c_int = 12;
pub const SQLITE_FULL: c_int = 13;
pub const SQLITE_CANTOPEN: c_int = 14;
pub const SQLITE_PROTOCOL: c_int = 15;
pub const SQLITE_EMPTY: c_int = 16;
pub const SQLITE_SCHEMA: c_int = 17;
pub const SQLITE_TOOBIG: c_int = 18;
pub const SQLITE_CONSTRAINT: c_int = 19;
pub const SQLITE_MISMATCH: c_int = 20;
pub const SQLITE_MISUSE: c_int = 21;
pub const SQLITE_RANGE: c_int = 25;
pub const SQLITE_NOTADB: c_int = 26;
/// SQLITE_ROW — `sqlite3_step` has another row ready.
pub const SQLITE_ROW: c_int = 100;
/// SQLITE_DONE — `sqlite3_step` finished the statement.
pub const SQLITE_DONE: c_int = 101;
// Extended codes (libsqlite3-sys reports these when
// sqlite3_extended_result_codes is on — sqlx always turns it on).
pub const SQLITE_BUSY_TIMEOUT: c_int = 5 | (2 << 8);
// Exact values from sqlite3.h (see compat/libsqlite3-sys bindings).
pub const SQLITE_CONSTRAINT_CHECK: c_int = 275;
pub const SQLITE_CONSTRAINT_NOTNULL: c_int = 1299;
pub const SQLITE_CONSTRAINT_PRIMARYKEY: c_int = 1555;
pub const SQLITE_CONSTRAINT_UNIQUE: c_int = 2067;
pub const SQLITE_CONSTRAINT_FOREIGNKEY: c_int = 787;
pub const SQLITE_IOERR_SHORT_READ: c_int = 10 | (1 << 8);

// Open flags.
pub const SQLITE_OPEN_READONLY: c_int = 0x0000_0001;
pub const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
pub const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
pub const SQLITE_OPEN_URI: c_int = 0x0000_0040;
pub const SQLITE_OPEN_MEMORY: c_int = 0x0000_0080;
pub const SQLITE_OPEN_NOMUTEX: c_int = 0x0000_8000;
pub const SQLITE_OPEN_FULLMUTEX: c_int = 0x0001_0000;
pub const SQLITE_OPEN_SHAREDCACHE: c_int = 0x0002_0000;
pub const SQLITE_OPEN_PRIVATECACHE: c_int = 0x0004_0000;

pub const SQLITE_INTEGER: c_int = 1;
pub const SQLITE_FLOAT: c_int = 2;
pub const SQLITE_TEXT: c_int = 3;
pub const SQLITE_BLOB: c_int = 4;
pub const SQLITE_NULL: c_int = 5;

/// SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION
pub const SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION: c_int = 1012;

/// Version string reported by `sqlite3_libversion` / `SELECT sqlite_version()`.
pub const SQLITE_LIBVERSION: &str = "3.50.4";
/// Build identity for `sqlite3_source_id`.
const SOURCE_ID: &str = "2026-09-01 00:00:00 rustqlite-compat 0.1.0 (SQLite C ABI on rustqlite)";

// ---------------------------------------------------------------------------
// Opaque C types
// ---------------------------------------------------------------------------

/// Database connection handle (`sqlite3`).
#[repr(C)]
pub struct sqlite3 {
    _private: [u8; 0],
}

/// Prepared-statement handle (`sqlite3_stmt`).
#[repr(C)]
pub struct sqlite3_stmt {
    _private: [u8; 0],
}

/// Value handle (`sqlite3_value`) — same layout as the plugin ABI's
/// `RqlValue`: a `Box<Value>`. Function callbacks registered via
/// `sqlite3_create_function_v2` receive pointers of this shape (the
/// engine's plugin trampolines construct them), and `sqlite3_column_value`
/// hands out the same representation.
#[repr(C)]
pub struct sqlite3_value {
    _private: [u8; 0],
}

/// Function-call context (`sqlite3_context`) — the plugin ABI's `CallCtx`.
#[repr(C)]
pub struct sqlite3_context {
    _private: [u8; 0],
}

// ---------------------------------------------------------------------------
// Engine sharing: one rustqlite engine per database file per process
// ---------------------------------------------------------------------------

/// A shared engine instance (one per canonical database file path).
struct Engine {
    db: parking_lot::RwLock<Database>,
    /// Connection id that currently owns the engine-level transaction
    /// (0 = none). Serializes cross-connection transactions with
    /// SQLite-style BUSY/busy_timeout semantics.
    tx_owner: AtomicUsize,
}

impl Engine {
    fn total_changes(&self) -> i64 {
        self.db.read().total_changes()
    }
    fn last_rowid(&self) -> i64 {
        self.db.read().last_insert_rowid()
    }
}

fn engines() -> &'static StdMutex<HashMap<String, Arc<Engine>>> {
    static ENGINES: OnceLock<StdMutex<HashMap<String, Arc<Engine>>>> = OnceLock::new();
    ENGINES.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Next connection id (for tx_owner bookkeeping).
fn next_conn_id() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

struct ConnState {
    /// True between BEGIN and COMMIT/ROLLBACK (per CONNECTION — what
    /// `sqlite3_get_autocommit` reports).
    in_tx: AtomicBool,
    busy_timeout_ms: AtomicI64,
    /// `sqlite3_changes` — row count of the most recently completed
    /// statement on this connection.
    changes: AtomicI64,
    total_changes: AtomicI64,
    last_rowid: AtomicI64,
    err_code: AtomicI64,
    err_msg: StdMutex<CString>,
}

/// Real connection object behind `sqlite3*`. `engine` is None only for
/// failed opens (the handle exists so `sqlite3_errmsg` works, like SQLite).
struct Conn {
    id: usize,
    engine: Option<Arc<Engine>>,
    /// True when the connection was opened read-only (SQLITE_OPEN_READONLY
    /// or `mode=ro`): preparing a database-mutating statement returns
    /// SQLITE_READONLY (SQLite's readonly-connection semantics).
    readonly: bool,
    state: ConnState,
    live_statements: AtomicUsize,
    /// Progress handler: (n_ops, callback, user ptr).
    progress: StdMutex<(
        c_int,
        Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
        *mut c_void,
    )>,
    /// Hooks: update / commit / rollback.
    /// (C callback, user-data pointer) pairs — raw pointers are the sqlite3
    /// hook ABI; each slot is mutex-guarded.
    update_hook: StdMutex<UpdateHook>,
    commit_hook: StdMutex<
        Option<(
            *mut c_void,
            Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
        )>,
    >,
    rollback_hook: StdMutex<Option<(*mut c_void, Option<unsafe extern "C" fn(*mut c_void)>)>>,
    /// Collations / functions are registered on the SHARED engine — with
    /// one engine per file they are visible to every connection of that
    /// file, which matches shared-cache SQLite closely enough for sqlx.
    _unused: (),
}

// SAFETY: `Conn` is handed to C as `sqlite3*` (a raw pointer, where
// Send/Sync are meaningless to C). Rust-side, the only fields carrying
// raw pointers are the C callback slots (progress/hooks), each guarded by
// a StdMutex, and all database state lives behind the engine's own
// RwLock. SQLite's default SQLITE_THREADSAFE=1 (serialized) builds allow
// a connection to be used from multiple threads serially — which is
// exactly what the `unsafe impl`s declare, and what sqlx-sqlite relies on
// when it moves the handle to its worker thread.
unsafe impl Send for Conn {}
unsafe impl Sync for Conn {}

/// sqlite3_update_hook slot: (user-data, callback) — the raw pointers are
/// the C hook ABI, guarded by a StdMutex on the connection.
type UpdateHook = Option<(
    *mut c_void,
    Option<unsafe extern "C" fn(*mut c_void, c_int, *const c_char, *const c_char, i64)>,
)>;

impl Conn {
    fn set_err(&self, code: c_int, msg: &str) -> c_int {
        self.state.err_code.store(code as i64, Ordering::Release);
        let c =
            CString::new(msg.replace('\0', " ")).unwrap_or_else(|_| CString::new("error").unwrap());
        *self.state.err_msg.lock().unwrap() = c;
        code
    }
    fn clear_err(&self) {
        self.state
            .err_code
            .store(SQLITE_OK as i64, Ordering::Release);
        *self.state.err_msg.lock().unwrap() = CString::new("").unwrap();
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Auto-rollback of an abandoned transaction (SQLite rolls back at
        // close when the connection had an open tx).
        if let Some(engine) = self
            .engine
            .as_ref()
            .filter(|_| self.state.in_tx.load(Ordering::Acquire))
            .filter(|e| e.tx_owner.load(Ordering::Acquire) == self.id)
        {
            let _ = engine.db.write().execute("ROLLBACK", []);
            engine.tx_owner.store(0, Ordering::Release);
        }
    }
}

// ---------------------------------------------------------------------------
// Statement classification
// ---------------------------------------------------------------------------

/// What a prepared statement does, decided from the AST.
enum StmtKind {
    /// SELECT / EXPLAIN — engine streaming statement.
    Select,
    /// INSERT / UPDATE / DELETE — engine statement (may have RETURNING).
    Dml { returning: bool },
    /// PRAGMA with a value (write form) — executed via Database::execute.
    PragmaWrite,
    /// PRAGMA without a value (read form) — executed via query at first
    /// step, rows buffered.
    PragmaRead,
    /// BEGIN / COMMIT / ROLLBACK / SAVEPOINT / RELEASE / DDL / ATTACH /
    /// VACUUM / ALTER — executed once via Database::execute.
    Once,
}

fn classify(stmt: &rustqlite::sql::ast::Statement) -> StmtKind {
    use rustqlite::sql::ast::Statement as S;
    match stmt {
        S::Select(_) => StmtKind::Select,
        S::Explain(_) => StmtKind::Select,
        S::Insert(i) => StmtKind::Dml {
            returning: i.returning.is_some(),
        },
        S::Update(u) => StmtKind::Dml {
            returning: u.returning.is_some(),
        },
        S::Delete(d) => StmtKind::Dml {
            returning: d.returning.is_some(),
        },
        S::Pragma(p) => {
            if p.value.is_some() {
                StmtKind::PragmaWrite
            } else {
                StmtKind::PragmaRead
            }
        }
        _ => StmtKind::Once,
    }
}

/// Is this statement a transaction-control statement?
fn is_tx_control(stmt: &rustqlite::sql::ast::Statement) -> bool {
    use rustqlite::sql::ast::Statement as S;
    matches!(
        stmt,
        S::Begin(_) | S::Commit | S::Rollback(_) | S::Savepoint(_) | S::Release(_)
    )
}

// ---------------------------------------------------------------------------
// Statement handle
// ---------------------------------------------------------------------------

/// How the statement executes.
enum Exec {
    /// Engine statement (SELECT/DML), lifetime-erased; valid because the
    /// owning engine (behind the shared Arc) outlives the connection, and
    /// `sqlite3_close` refuses while statements are live.
    Rows(Box<EngineStatement<'static>>),
    /// Executed once on first step via the mutable path.
    Once { sql: String },
    /// Read-pragma / query form: run `Database::query_with_columns` on the
    /// first step and serve the buffered rows.
    Query {
        sql: String,
        rows: std::vec::IntoIter<Vec<Value>>,
        columns: Vec<String>,
    },
}

struct Stmt {
    conn: Arc<Conn>,
    /// Connection's engine (copied so finalize never derefs a dangling
    /// conn pointer).
    engine: Option<Arc<Engine>>,
    sql: CString,
    kind: StmtKind,
    exec: Exec,
    /// True for statements that write (take the engine write lock).
    is_write: bool,
    /// SQLite parameter table: one entry per `?`/`:name`/`@name`/`$var`
    /// parameter in order of appearance. Index (1-based) → slot.
    params: Vec<ParamSlot>,
    /// Static column names (DML RETURNING / PragmaRead / Query shapes).
    static_columns: Vec<CString>,
    /// Values handed out by `sqlite3_column_value` — freed at the next
    /// step / reset / finalize (SQLite: unprotected value objects).
    value_pool: Vec<*mut sqlite3_value>,
    /// NUL-terminated text buffer for column_text/value_text (valid until
    /// the next accessor call on this statement).
    text_buf: Vec<u8>,
    /// Error code of the most recent step (sqlite3_reset returns it).
    last_step_err: c_int,
    /// True once the first step ran.
    stepped: bool,
    done: bool,
    /// total_changes snapshot at the last step's start (for changes).
    total_at_step_start: i64,
    /// Exec::Query: the query ran on the first step.
    query_ran: bool,
    /// update_hook fired for this execution already.
    hook_fired: bool,
    /// changes() already reported for this execution (prevents the final
    /// Done step of a RETURNING statement from clobbering it with 0 —
    /// SQLite keeps the statement's change count until the next run).
    changes_reported: bool,
    /// Exec::Query: the row being served.
    current_row: Option<Vec<Value>>,
}

enum ParamSlot {
    /// Positional: engine positional index (0-based).
    Positional(usize),
    /// Named: engine named-parameter key (with sigil).
    Named(String),
}

// ---------------------------------------------------------------------------
// Error → SQLite result code mapping
// ---------------------------------------------------------------------------

/// Map an engine error to a SQLite result code, using the message shape
/// (SQLite-compatible constraint messages) plus the error taxonomy.
fn engine_err_code(e: &rustqlite::Error) -> c_int {
    let msg = e.to_string();
    if msg.contains("UNIQUE constraint failed") {
        return SQLITE_CONSTRAINT_UNIQUE;
    }
    if msg.contains("NOT NULL constraint failed") {
        return SQLITE_CONSTRAINT_NOTNULL;
    }
    if msg.contains("FOREIGN KEY constraint failed") {
        return SQLITE_CONSTRAINT_FOREIGNKEY;
    }
    if msg.contains("CHECK constraint failed") {
        return SQLITE_CONSTRAINT_CHECK;
    }
    let _ = &msg;
    use rustqlite::Error as E;
    match e {
        E::Io(io) => {
            if io.kind() == std::io::ErrorKind::PermissionDenied {
                SQLITE_PERM
            } else if io.kind() == std::io::ErrorKind::NotFound {
                SQLITE_CANTOPEN
            } else {
                SQLITE_IOERR
            }
        }
        E::Corruption(_) | E::Btree(_) | E::Wal(_) => SQLITE_CORRUPT,
        E::Constraint(msg) => {
            // Fast path on the variant itself (no string scan).
            if msg.contains("UNIQUE constraint failed") {
                SQLITE_CONSTRAINT_UNIQUE
            } else if msg.contains("NOT NULL constraint failed") {
                SQLITE_CONSTRAINT_NOTNULL
            } else if msg.contains("FOREIGN KEY constraint failed") {
                SQLITE_CONSTRAINT_FOREIGNKEY
            } else if msg.contains("CHECK constraint failed") {
                SQLITE_CONSTRAINT_CHECK
            } else if msg.contains("datatype mismatch") {
                // SQLite: SQLITE_MISMATCH for rowid-alias type errors.
                SQLITE_MISMATCH
            } else {
                SQLITE_CONSTRAINT
            }
        }
        E::Transaction(_) => SQLITE_BUSY,
        E::Lex { .. } | E::Parse { .. } => SQLITE_ERROR,
        E::Semantic(_) => SQLITE_ERROR, // already-exists and other semantic errors share SQLITE_ERROR
        E::NotFound(_) => SQLITE_ERROR,
        E::AlreadyExists(_) => SQLITE_ERROR,
        E::Planner(_) | E::Runtime(_) => SQLITE_ERROR,
        E::Unsupported(_) => SQLITE_ERROR,
        E::InvalidArgument(_) => SQLITE_MISUSE,
    }
}

fn set_conn_err(conn: &Conn, e: &rustqlite::Error) -> c_int {
    conn.set_err(engine_err_code(e), &engine_err_msg(e))
}

/// Would executing this AST mutate the database? (Read-only connection
/// enforcement — see `Conn::readonly`.)
fn ast_mutates_database(stmt: &rustqlite::sql::ast::Statement) -> bool {
    use rustqlite::sql::ast::Statement as S;
    match stmt {
        S::Select(_) | S::Explain(_) => false,
        S::Begin(_) | S::Commit | S::Rollback(_) | S::Savepoint(_) | S::Release(_) => false,
        S::Pragma(p) => p.value.is_some(),
        // INSERT / UPDATE / DELETE / CREATE / DROP / ALTER / ATTACH /
        // DETACH / VACUUM all mutate.
        _ => true,
    }
}

/// The user-visible message WITHOUT the engine's taxonomy prefix
/// ("semantic error: ..." etc.) — SQLite's errmsg has no such prefix.
fn engine_err_msg(e: &rustqlite::Error) -> String {
    use rustqlite::Error as E;
    match e {
        E::Semantic(s) => s.clone(),
        E::NotFound(s) => s.clone(),
        E::AlreadyExists(s) => s.clone(),
        E::Runtime(s) => s.clone(),
        E::Planner(s) => s.clone(),
        E::InvalidArgument(s) => s.clone(),
        E::Corruption(s) => s.clone(),
        E::Constraint(s) => s.clone(),
        E::Transaction(s) => s.clone(),
        E::Btree(s) => s.clone(),
        E::Wal(s) => s.clone(),
        _ => e.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Script scanning: first-statement end (SQLite's pzTail semantics)
// ---------------------------------------------------------------------------

/// Byte offset just past the first top-level `;` (outside strings and
/// comments), or the end of input. Skips nothing else.
fn scan_stmt_end(sql: &[u8]) -> usize {
    let mut i = 0usize;
    while i < sql.len() {
        let c = sql[i];
        match c {
            b'\'' | b'"' => {
                let quote = c;
                i += 1;
                while i < sql.len() {
                    if sql[i] == quote {
                        if i + 1 < sql.len() && sql[i + 1] == quote {
                            i += 2; // escaped quote ('' or "")
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if i + 1 < sql.len() && sql[i + 1] == b'-' => {
                while i < sql.len() && sql[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < sql.len() && sql[i + 1] == b'*' => {
                i += 2;
                while i + 1 < sql.len() && !(sql[i] == b'*' && sql[i + 1] == b'/') {
                    i += 1;
                }
                i += 2.min(sql.len() - i);
            }
            b';' => return i + 1,
            _ => i += 1,
        }
    }
    sql.len()
}

/// True if a script chunk is only whitespace / comments / `;`.
fn is_blank_chunk(s: &str) -> bool {
    s.chars().all(|c| c.is_whitespace() || c == ';')
}

/// Find the first non-blank statement: returns (statement_text, tail_offset).
/// statement_text is None when the whole script is blank.
fn split_first_stmt(sql: &str) -> (Option<String>, usize) {
    let bytes = sql.as_bytes();
    let mut pos = 0usize;
    loop {
        // Skip leading whitespace / comments by advancing through blank
        // characters. Simple approach: find the next `;` end; if the chunk
        // before it is blank, continue after it.
        let end = scan_stmt_end(&bytes[pos..]) + pos;
        let chunk = &sql[pos..end];
        if is_blank_chunk(chunk.trim_end_matches(';')) {
            if end >= sql.len() {
                return (None, sql.len());
            }
            pos = end;
            continue;
        }
        // Trim the trailing ';' (and whitespace before it) from the text.
        let stmt_text = chunk.trim_end();
        let stmt_text = stmt_text.strip_suffix(';').unwrap_or(stmt_text).trim_end();
        return (Some(stmt_text.to_string()), end);
    }
}

// ---------------------------------------------------------------------------
// URI filename parsing (SQLITE_OPEN_URI)
// ---------------------------------------------------------------------------

struct UriInfo {
    path: String,
    mode_memory: bool,
    cache_shared: bool,
    readonly: bool,
}

fn parse_uri(filename: &str) -> Option<UriInfo> {
    let rest = filename.strip_prefix("file:")?;
    // Split path from query.
    let (path, query) = match rest.find('?') {
        Some(q) => (&rest[..q], &rest[q + 1..]),
        None => (rest, ""),
    };
    let mut mode_memory = false;
    let mut cache_shared = false;
    let mut readonly = false;
    for kv in query.split('&') {
        let (k, v) = match kv.split_once('=') {
            Some((k, v)) => (k, v),
            None => (kv, ""),
        };
        match k {
            "mode" => match v {
                "memory" => mode_memory = true,
                "rw" | "rwc" => {}
                "ro" => readonly = true,
                _ => {}
            },
            "cache" => cache_shared = v == "shared",
            _ => {}
        }
    }
    Some(UriInfo {
        path: percent_decode(path),
        mode_memory,
        cache_shared,
        readonly,
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() + 1 && i + 2 < b.len() + 1 {
            if let (Some(h), Some(l)) = (
                b.get(i + 1).and_then(|c| (*c as char).to_digit(16)),
                b.get(i + 2).and_then(|c| (*c as char).to_digit(16)),
            ) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve which engine a connection should use and how.
enum OpenTarget {
    /// `:memory:` — private engine.
    PrivateMemory,
    /// Shared engine keyed by this string.
    Shared(String),
    /// Ordinary file.
    File(PathBuf),
}

fn resolve_open_target(filename: &str, flags: c_int) -> Result<(OpenTarget, bool), String> {
    let uri = if flags & SQLITE_OPEN_URI != 0 {
        parse_uri(filename)
    } else {
        None
    };
    if let Some(u) = uri {
        if u.mode_memory {
            if u.cache_shared {
                return Ok((OpenTarget::Shared(format!("mem:{}", u.path)), u.readonly));
            }
            return Ok((OpenTarget::PrivateMemory, u.readonly));
        }
        if u.path.is_empty() {
            return Ok((OpenTarget::PrivateMemory, u.readonly));
        }
        return Ok((OpenTarget::File(PathBuf::from(u.path)), u.readonly));
    }
    if filename == ":memory:" {
        return Ok((OpenTarget::PrivateMemory, false));
    }
    if filename.is_empty() {
        // SQLite: empty filename = a temporary on-disk database. We map it
        // to a private temp file.
        let mut p = std::env::temp_dir();
        p.push(format!("rustqlite-anon-{}.db", std::process::id()));
        return Ok((OpenTarget::File(p), false));
    }
    Ok((OpenTarget::File(PathBuf::from(filename)), false))
}

fn open_engine(target: &OpenTarget, create: bool, readonly: bool) -> Result<Database, String> {
    let db = match target {
        OpenTarget::PrivateMemory => Database::open_in_memory(),
        OpenTarget::Shared(key) => {
            // Named shared in-memory database.
            if key.starts_with("mem:") {
                Database::open_in_memory()
            } else {
                Database::open(key)
            }
        }
        OpenTarget::File(p) => {
            if !p.as_os_str().is_empty() && !p.exists() {
                if !create {
                    return Err("unable to open database file".to_string());
                }
                if let Some(parent) = p.parent() {
                    if !parent.as_os_str().is_empty() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                }
            }
            Database::open(p.to_str().unwrap_or_default())
        }
    };
    let db = db.map_err(|e| e.to_string())?;
    if readonly {
        // Advisory: the engine has no readonly mode flag yet; a readonly
        // open of a writable file still permits writes (documented).
    }
    Ok(db)
}

fn acquire_engine(
    target: &OpenTarget,
    create: bool,
    readonly: bool,
) -> Result<(Option<Arc<Engine>>, bool), String> {
    match target {
        OpenTarget::PrivateMemory => {
            let db = open_engine(target, create, readonly)?;
            Ok((
                Some(Arc::new(Engine {
                    db: parking_lot::RwLock::new(db),
                    tx_owner: AtomicUsize::new(0),
                })),
                true,
            ))
        }
        OpenTarget::Shared(key) => {
            let mut map = engines().lock().unwrap();
            if let Some(e) = map.get(key) {
                Ok((Some(e.clone()), false))
            } else {
                let db = open_engine(target, create, readonly)?;
                let e = Arc::new(Engine {
                    db: parking_lot::RwLock::new(db),
                    tx_owner: AtomicUsize::new(0),
                });
                map.insert(key.clone(), e.clone());
                Ok((Some(e), false))
            }
        }
        OpenTarget::File(p) => {
            let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            let key = key.to_string_lossy().into_owned();
            let mut map = engines().lock().unwrap();
            if let Some(e) = map.get(&key) {
                Ok((Some(e.clone()), false))
            } else {
                let db = open_engine(target, create, readonly)?;
                let e = Arc::new(Engine {
                    db: parking_lot::RwLock::new(db),
                    tx_owner: AtomicUsize::new(0),
                });
                map.insert(key, e.clone());
                Ok((Some(e), false))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Connection lifecycle
// ---------------------------------------------------------------------------

fn new_conn(engine: Option<Arc<Engine>>, readonly: bool) -> Arc<Conn> {
    Arc::new(Conn {
        id: next_conn_id(),
        engine,
        readonly,
        state: ConnState {
            in_tx: AtomicBool::new(false),
            busy_timeout_ms: AtomicI64::new(0),
            changes: AtomicI64::new(0),
            total_changes: AtomicI64::new(0),
            last_rowid: AtomicI64::new(0),
            err_code: AtomicI64::new(SQLITE_OK as i64),
            err_msg: StdMutex::new(CString::new("").unwrap()),
        },
        live_statements: AtomicUsize::new(0),
        progress: StdMutex::new((0, None, std::ptr::null_mut())),
        update_hook: StdMutex::new(None),
        commit_hook: StdMutex::new(None),
        rollback_hook: StdMutex::new(None),
        _unused: (),
    })
}

unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok()
}

/// sqlite3_open
#[no_mangle]
pub unsafe extern "C" fn sqlite3_open(filename: *const c_char, ppdb: *mut *mut sqlite3) -> c_int {
    sqlite3_open_v2(
        filename,
        ppdb,
        SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
        std::ptr::null(),
    )
}

/// sqlite3_open_v2 — flags honored: URI parsing, MEMORY, READONLY without
/// CREATE (missing file → SQLITE_CANTOPEN). NOMUTEX / FULLMUTEX /
/// SHAREDCACHE / PRIVATECACHE are accepted (the engine is always
/// thread-safe; connections share one engine per file either way).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_open_v2(
    filename: *const c_char,
    ppdb: *mut *mut sqlite3,
    flags: c_int,
    _z_vfs: *const c_char,
) -> c_int {
    if ppdb.is_null() {
        return SQLITE_MISUSE;
    }
    *ppdb = std::ptr::null_mut();
    let name = match cstr(filename) {
        Some(s) => s,
        None => return SQLITE_MISUSE,
    };
    let (target, uri_readonly) = match resolve_open_target(name, flags) {
        Ok(v) => v,
        Err(e) => {
            // Error-state handle so sqlite3_errmsg works (SQLite behavior).
            let conn = new_conn(None, false);
            conn.set_err(SQLITE_CANTOPEN, &e);
            *ppdb = Arc::into_raw(conn) as *mut sqlite3;
            return SQLITE_CANTOPEN;
        }
    };
    let create = flags & SQLITE_OPEN_CREATE != 0;
    // Read-only when either the open flag or the URI's mode=ro says so.
    let readonly = (flags & SQLITE_OPEN_READONLY != 0) || uri_readonly;
    let (engine, _private) = match acquire_engine(&target, create, readonly) {
        Ok(v) => v,
        Err(e) => {
            let conn = new_conn(None, false);
            let code = SQLITE_CANTOPEN;
            conn.set_err(code, &e);
            *ppdb = Arc::into_raw(conn) as *mut sqlite3;
            return code;
        }
    };
    if let Some(e) = &engine {
        // Seed connection counters from the shared engine.
        let _ = e.total_changes();
    }
    let conn = new_conn(engine, readonly);
    *ppdb = Arc::into_raw(conn) as *mut sqlite3;
    SQLITE_OK
}

/// sqlite3_close — SQLITE_BUSY while statements are live (sqlx relies on
/// this: it finalizes everything first).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_close(db: *mut sqlite3) -> c_int {
    sqlite3_close_v2(db)
}

/// sqlite3_close_v2 — same behavior (finalize-then-close is the caller's
/// job; with live statements we report BUSY and leak nothing).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_close_v2(db: *mut sqlite3) -> c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    let conn = &*(db as *const Conn);
    if conn.live_statements.load(Ordering::Acquire) > 0 {
        return conn.set_err(SQLITE_BUSY, "unable to close due to unfinalized statements");
    }
    drop(Arc::from_raw(db as *mut Conn));
    SQLITE_OK
}

// ---------------------------------------------------------------------------
// Error accessors
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char {
    if db.is_null() {
        return c"".as_ptr();
    }
    let conn = &*(db as *const Conn);
    let code = conn.state.err_code.load(Ordering::Acquire);
    if code == 0 {
        // Fall back to the last engine error, if any.
        return conn.state.err_msg.lock().unwrap().as_ptr();
    }
    conn.state.err_msg.lock().unwrap().as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_errcode(db: *mut sqlite3) -> c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    let conn = &*(db as *const Conn);
    conn.state.err_code.load(Ordering::Acquire) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_extended_errcode(db: *mut sqlite3) -> c_int {
    sqlite3_errcode(db)
}

/// sqlite3_errstr — static message for a result code.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_errstr(code: c_int) -> *const c_char {
    let msg: &'static str = match code {
        SQLITE_OK => "not an error",
        SQLITE_ERROR => "SQL logic error",
        SQLITE_BUSY => "database is locked",
        SQLITE_LOCKED => "database table is locked",
        SQLITE_NOMEM => "out of memory",
        SQLITE_READONLY => "attempt to write a readonly database",
        SQLITE_INTERRUPT => "interrupted",
        SQLITE_IOERR => "disk I/O error",
        SQLITE_CORRUPT => "database disk image is malformed",
        SQLITE_NOTFOUND => "unknown operation",
        SQLITE_FULL => "database or disk is full",
        SQLITE_CANTOPEN => "unable to open database file",
        SQLITE_CONSTRAINT => "constraint failed",
        SQLITE_MISUSE => "bad parameter or other API misuse",
        SQLITE_RANGE => "column index out of range",
        SQLITE_NOTADB => "file is not a database",
        SQLITE_ROW => "another row available",
        SQLITE_DONE => "another row available", // SQLite's quirk: "another SQLITE_DONE"
        _ => "unknown error code",
    };
    // Leak-once static table: code → CString. Small and bounded.
    static TABLE: OnceLock<StdMutex<HashMap<c_int, CString>>> = OnceLock::new();
    let t = TABLE.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut t = t.lock().unwrap();
    if let Some(c) = t.get(&code) {
        c.as_ptr()
    } else {
        let c = CString::new(msg).unwrap();
        let p = c.as_ptr();
        t.insert(code, c);
        p
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_extended_result_codes(_db: *mut sqlite3, _on: c_int) -> c_int {
    SQLITE_OK // always on
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_libversion() -> *const c_char {
    static V: OnceLock<CString> = OnceLock::new();
    V.get_or_init(|| CString::new(SQLITE_LIBVERSION).unwrap())
        .as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_libversion_number() -> c_int {
    // 3.50.4
    3 * 1_000_000 + 50 * 1_000 + 4
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_source_id() -> *const c_char {
    static V: OnceLock<CString> = OnceLock::new();
    V.get_or_init(|| CString::new(SOURCE_ID).unwrap()).as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_threadsafe() -> c_int {
    1
}

// ---------------------------------------------------------------------------
// Connection state accessors
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3_changes(db: *mut sqlite3) -> c_int {
    if db.is_null() {
        return 0;
    }
    let conn = &*(db as *const Conn);
    conn.state.changes.load(Ordering::Acquire) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_changes64(db: *mut sqlite3) -> i64 {
    if db.is_null() {
        return 0;
    }
    let conn = &*(db as *const Conn);
    conn.state.changes.load(Ordering::Acquire)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_total_changes(db: *mut sqlite3) -> c_int {
    if db.is_null() {
        return 0;
    }
    let conn = &*(db as *const Conn);
    conn.state.total_changes.load(Ordering::Acquire) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_total_changes64(db: *mut sqlite3) -> i64 {
    if db.is_null() {
        return 0;
    }
    let conn = &*(db as *const Conn);
    conn.state.total_changes.load(Ordering::Acquire)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_last_insert_rowid(db: *mut sqlite3) -> i64 {
    if db.is_null() {
        return 0;
    }
    let conn = &*(db as *const Conn);
    conn.state.last_rowid.load(Ordering::Acquire)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_set_last_insert_rowid(db: *mut sqlite3, rowid: i64) {
    if db.is_null() {
        return;
    }
    let conn = &*(db as *const Conn);
    conn.state.last_rowid.store(rowid, Ordering::Release);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_get_autocommit(db: *mut sqlite3) -> c_int {
    if db.is_null() {
        return 0;
    }
    let conn = &*(db as *const Conn);
    if conn.state.in_tx.load(Ordering::Acquire) {
        0
    } else {
        1
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_busy_timeout(db: *mut sqlite3, ms: c_int) -> c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    let conn = &*(db as *const Conn);
    conn.state
        .busy_timeout_ms
        .store(ms as i64, Ordering::Release);
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_handle(stmt: *mut sqlite3_stmt) -> *mut sqlite3 {
    if stmt.is_null() {
        return std::ptr::null_mut();
    }
    let s = &*(stmt as *const Stmt);
    // The conn Box is stable and outlives the statement (close refuses
    // while statements are live).
    Arc::as_ptr(&s.conn) as *mut sqlite3
}

/// Sleep waiting for the engine-level transaction lock, honoring
/// busy_timeout. Returns true if acquired.
fn await_tx_slot(engine: &Arc<Engine>, conn_id: usize, timeout_ms: i64) -> bool {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
    loop {
        let owner = engine.tx_owner.load(Ordering::Acquire);
        if owner == 0 || owner == conn_id {
            return true;
        }
        if timeout_ms > 0 && std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------
// Parameter discovery: walk the AST in appearance order
// ---------------------------------------------------------------------------

fn collect_param_slots(stmt: &rustqlite::sql::ast::Statement) -> Vec<ParamSlot> {
    use rustqlite::sql::ast::Statement as S;
    let mut slots = Vec::new();
    let mut max_explicit = 0usize; // highest ?N seen
    match stmt {
        S::Select(s) => collect_select(s, &mut slots, &mut max_explicit),
        S::Insert(i) => {
            if let Some(cols) = &i.columns {
                for _ in cols {
                    // Column names can't hold parameters.
                }
            }
            match &i.source {
                rustqlite::sql::ast::InsertSource::Values(rows) => {
                    for row in rows {
                        for e in row {
                            walk_expr(e, &mut slots, &mut max_explicit);
                        }
                    }
                }
                rustqlite::sql::ast::InsertSource::Select(sel) => {
                    collect_select(sel, &mut slots, &mut max_explicit);
                }
                _ => {}
            }
            if let Some(u) = &i.upsert {
                if let rustqlite::sql::ast::UpsertAction::DoUpdate { set, .. } = &u.action {
                    for (_, e) in set {
                        walk_expr(e, &mut slots, &mut max_explicit);
                    }
                }
            }
            if let Some(rcs) = &i.returning {
                collect_result_columns(rcs, &mut slots, &mut max_explicit);
            }
        }
        S::Update(u) => {
            for (_, e) in &u.set {
                walk_expr(e, &mut slots, &mut max_explicit);
            }
            if let Some(w) = &u.where_clause {
                walk_expr(w, &mut slots, &mut max_explicit);
            }
            if let Some(rcs) = &u.returning {
                collect_result_columns(rcs, &mut slots, &mut max_explicit);
            }
        }
        S::Delete(d) => {
            if let Some(w) = &d.where_clause {
                walk_expr(w, &mut slots, &mut max_explicit);
            }
            if let Some(rcs) = &d.returning {
                collect_result_columns(rcs, &mut slots, &mut max_explicit);
            }
        }
        S::Pragma(rustqlite::sql::ast::PragmaStatement {
            value: Some(rustqlite::sql::ast::PragmaValue::Expr(e)),
            ..
        }) => walk_expr(e, &mut slots, &mut max_explicit),
        _ => {}
    }
    let _ = max_explicit;
    slots
}

fn collect_select(
    s: &rustqlite::sql::ast::SelectStatement,
    slots: &mut Vec<ParamSlot>,
    max_explicit: &mut usize,
) {
    use rustqlite::sql::ast::SelectBody;
    let body = match &s.body {
        SelectBody::Simple(sel) => sel,
        // Compound selects: walk the LEFT-most simple body only — the
        // parameter slots of a compound are the union anyway, and the
        // engine binds by index, so missing the right arm's ORDER is
        // corrected by the named/index mapping below.
        SelectBody::Binary { left, .. } => match left.as_ref() {
            SelectBody::Simple(sel) => sel,
            _ => return,
        },
    };
    for rc in &body.columns {
        if let rustqlite::sql::ast::ResultColumn::Expr { expr, .. } = rc {
            walk_expr(expr, slots, max_explicit);
        }
    }
    if let Some(w) = &body.where_clause {
        walk_expr(w, slots, max_explicit);
    }
    for t in &body.group_by {
        walk_expr(t, slots, max_explicit);
    }
    if let Some(h) = &body.having {
        walk_expr(h, slots, max_explicit);
    }
    // ORDER BY / LIMIT live on the SelectStatement, not the body.
    for o in &s.order_by {
        walk_expr(&o.expr, slots, max_explicit);
    }
    if let Some(l) = &s.limit {
        walk_expr(l, slots, max_explicit);
    }
    if let Some(off) = &s.offset {
        walk_expr(off, slots, max_explicit);
    }
}

fn collect_result_columns(
    rcs: &[rustqlite::sql::ast::ResultColumn],
    slots: &mut Vec<ParamSlot>,
    max_explicit: &mut usize,
) {
    for rc in rcs {
        if let rustqlite::sql::ast::ResultColumn::Expr { expr, .. } = rc {
            walk_expr(expr, slots, max_explicit);
        }
    }
}

fn walk_expr(e: &rustqlite::sql::ast::Expr, slots: &mut Vec<ParamSlot>, max_explicit: &mut usize) {
    use rustqlite::sql::ast::Expr;
    match e {
        Expr::Parameter(name) => {
            if name.chars().all(|c| c.is_ascii_digit()) && !name.is_empty() {
                // Anonymous `?` (lexer emits "0", "1", ...) or `?N` —
                // numeric names are the engine's positional Vec indices.
                let idx: usize = name.parse().unwrap_or(0);
                *max_explicit = (*max_explicit).max(idx + 1);
                slots.push(ParamSlot::Positional(idx));
            } else {
                // :name / @name / $name — keep the sigil (engine key form).
                slots.push(ParamSlot::Named(name.clone()));
            }
        }
        Expr::Binary { left, right, .. } => {
            walk_expr(left, slots, max_explicit);
            walk_expr(right, slots, max_explicit);
        }
        Expr::Unary { expr, .. } => walk_expr(expr, slots, max_explicit),
        Expr::Function { args, .. } => {
            for a in args {
                walk_expr(a, slots, max_explicit);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                walk_expr(o, slots, max_explicit);
            }
            for (w, t) in whens {
                walk_expr(w, slots, max_explicit);
                walk_expr(t, slots, max_explicit);
            }
            if let Some(el) = else_ {
                walk_expr(el, slots, max_explicit);
            }
        }
        Expr::In {
            source: rustqlite::sql::ast::InSource::List(items),
            ..
        } => {
            for i in items {
                walk_expr(i, slots, max_explicit);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            walk_expr(expr, slots, max_explicit);
            walk_expr(low, slots, max_explicit);
            walk_expr(high, slots, max_explicit);
        }
        Expr::Cast { expr, .. } => walk_expr(expr, slots, max_explicit),
        Expr::Collate { expr, .. } => walk_expr(expr, slots, max_explicit),
        Expr::Exists(..) | Expr::Subquery(..) => {}
        Expr::IsNull { expr, .. } => walk_expr(expr, slots, max_explicit),
        Expr::Row(items) => {
            for i in items {
                walk_expr(i, slots, max_explicit);
            }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            walk_expr(expr, slots, max_explicit);
            walk_expr(pattern, slots, max_explicit);
            if let Some(es) = escape {
                walk_expr(es, slots, max_explicit);
            }
        }
        Expr::Is { left, right, .. } => {
            walk_expr(left, slots, max_explicit);
            walk_expr(right, slots, max_explicit);
        }
        _ => {}
    }
}

/// SQLite-style name for a RETURNING / projection column.
fn result_column_name(rc: &rustqlite::sql::ast::ResultColumn) -> String {
    use rustqlite::sql::ast::ResultColumn;
    match rc {
        ResultColumn::Star => "*".to_string(),
        ResultColumn::TableStar(t) => format!("{}.*", t),
        ResultColumn::Expr { expr, alias } => {
            if let Some(a) = alias {
                a.clone()
            } else {
                rustqlite::executor::expr_display_name(expr)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// prepare
// ---------------------------------------------------------------------------

/// Shared implementation for prepare_v2 / prepare_v3.
unsafe fn prepare_impl(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    _flags: c_int, // SQLITE_PREPARE_PERSISTENT etc. — always persistent here
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    let conn: &Conn = match (db as *const Conn).as_ref() {
        Some(c) => c,
        None => return SQLITE_MISUSE,
    };
    if pp_stmt.is_null() {
        return SQLITE_MISUSE;
    }
    *pp_stmt = std::ptr::null_mut();
    if z_sql.is_null() {
        return conn.set_err(SQLITE_MISUSE, "invalid statement text");
    }
    let sql_str: String = if n_byte >= 0 {
        // Explicit length: the buffer may not be NUL-terminated.
        let bytes = std::slice::from_raw_parts(z_sql as *const u8, n_byte as usize);
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        match cstr(z_sql) {
            Some(s) => s.to_string(),
            None => return conn.set_err(SQLITE_MISUSE, "invalid statement text"),
        }
    };
    let engine = match &conn.engine {
        Some(e) => e.clone(),
        None => return conn.set_err(SQLITE_CANTOPEN, "connection is in error state"),
    };

    // Split the first statement off the script (pzTail semantics).
    let (stmt_text, tail_off) = split_first_stmt(&sql_str);
    if !pz_tail.is_null() {
        *pz_tail = z_sql.add(tail_off.min(sql_str.len()));
    }
    let Some(stmt_text) = stmt_text else {
        // Blank / comments only: no statement.
        return SQLITE_OK;
    };

    // Parse + classify via the engine's own parser.
    let ast = match rustqlite::sql::parser::parse(&stmt_text) {
        Ok(a) => a,
        Err(e) => return set_conn_err(conn, &e),
    };
    // Read-only connections reject database-mutating statements at
    // PREPARE (SQLite's SQLITE_READONLY for a readonly connection —
    // transaction control and SELECTs are unaffected).
    if conn.readonly && ast_mutates_database(&ast) {
        return conn.set_err(SQLITE_READONLY, "attempt to write a readonly database");
    }
    let kind = classify(&ast);
    let is_write = matches!(
        kind,
        StmtKind::Dml { .. } | StmtKind::PragmaWrite | StmtKind::Once
    );
    let params = collect_param_slots(&ast);

    let mut static_columns: Vec<CString> = Vec::new();
    let exec: Exec = match &kind {
        StmtKind::Select | StmtKind::Dml { .. } => {
            let eng_stmt = {
                let rd = engine.db.read();
                // Bind to a local first: using the `match` as the block's
                // tail expression keeps the `Result` temporary alive until
                // after `rd` drops (E0597 on newer rustc); a `let` statement
                // drops the scrutinee temporary before the guard does.
                let erased = match rd.prepare(&stmt_text) {
                    Ok(s) => {
                        // Lifetime-erasure happens under the guard: the
                        // Database lives in the Arc'd RwLock, so the
                        // reference remains valid for the engine's life.
                        let erased: EngineStatement<'static> = std::mem::transmute(s);
                        erased
                    }
                    Err(e) => return set_conn_err(conn, &e),
                };
                erased
            };
            // Prepare-time column names (sqlx reads column_count /
            // column_name BEFORE the first step):
            //  1. the engine knows them when a streaming driver covers the
            //     plan (the hot OLTP shapes);
            //  2. DML: 0 columns without RETURNING, AST names with;
            //  3. otherwise (materialized SELECT / EXPLAIN) execute once
            //     now and reset (side-effect free — SELECTs only).
            if let StmtKind::Dml { returning } = &kind {
                if *returning {
                    let rcs: Option<&Vec<rustqlite::sql::ast::ResultColumn>> = match &ast {
                        rustqlite::sql::ast::Statement::Insert(i) => i.returning.as_ref(),
                        rustqlite::sql::ast::Statement::Update(u) => u.returning.as_ref(),
                        rustqlite::sql::ast::Statement::Delete(d) => d.returning.as_ref(),
                        _ => None,
                    };
                    for rc in rcs.into_iter().flatten() {
                        if let Ok(c) = CString::new(result_column_name(rc)) {
                            static_columns.push(c);
                        }
                    }
                }
            } else if eng_stmt.column_count() > 0 {
                for i in 0..eng_stmt.column_count() {
                    let name = eng_stmt.column_name(i).unwrap_or("").to_string();
                    if let Ok(c) = CString::new(name) {
                        static_columns.push(c);
                    }
                }
            } else {
                let mut s = eng_stmt;
                {
                    let _rd = engine.db.read();
                    let _ = s.step();
                }
                for i in 0..s.column_count() {
                    let name = s.column_name(i).unwrap_or("").to_string();
                    if let Ok(c) = CString::new(name) {
                        static_columns.push(c);
                    }
                }
                s.reset();
                let erased: EngineStatement<'static> = std::mem::transmute(s);
                return finish_prepare(
                    conn,
                    engine,
                    stmt_text,
                    kind,
                    is_write,
                    Exec::Rows(Box::new(erased)),
                    params,
                    static_columns,
                    pp_stmt,
                );
            }
            let erased: EngineStatement<'static> = std::mem::transmute(eng_stmt);
            Exec::Rows(Box::new(erased))
        }
        StmtKind::PragmaRead => {
            let name = match &ast {
                rustqlite::sql::ast::Statement::Pragma(p) => p.name.clone(),
                _ => String::new(),
            };
            if let Ok(c) = CString::new(name) {
                static_columns.push(c);
            }
            Exec::Query {
                sql: stmt_text.clone(),
                rows: Vec::new().into_iter(),
                columns: Vec::new(),
            }
        }
        StmtKind::PragmaWrite | StmtKind::Once => Exec::Once {
            sql: stmt_text.clone(),
        },
    };
    finish_prepare(
        conn,
        engine,
        stmt_text,
        kind,
        is_write,
        exec,
        params,
        static_columns,
        pp_stmt,
    )
}

/// Build the statement handle and store it at `*pp_stmt`.
#[allow(clippy::too_many_arguments)]
unsafe fn finish_prepare(
    conn: &Conn,
    engine: Arc<Engine>,
    stmt_text: String,
    kind: StmtKind,
    is_write: bool,
    exec: Exec,
    params: Vec<ParamSlot>,
    static_columns: Vec<CString>,
    pp_stmt: *mut *mut sqlite3_stmt,
) -> c_int {
    let owned = match CString::new(stmt_text) {
        Ok(c) => c,
        Err(_) => return conn.set_err(SQLITE_MISUSE, "statement contains NUL bytes"),
    };
    // Clone the owning Arc<Conn> WITHOUT stealing the C side's reference:
    // from_raw + clone + forget bumps the count by exactly one (the
    // statement's own), leaving open's Arc::into_raw count intact.
    let conn_arc: Arc<Conn> = {
        let borrowed = unsafe { Arc::from_raw(conn as *const Conn) };
        let cloned = borrowed.clone();
        std::mem::forget(borrowed);
        cloned
    };
    let stmt = Box::new(Stmt {
        conn: conn_arc,
        engine: Some(engine),
        sql: owned,
        kind,
        exec,
        is_write,
        params,
        static_columns,
        value_pool: Vec::new(),
        text_buf: Vec::new(),
        last_step_err: SQLITE_OK,
        stepped: false,
        done: false,
        total_at_step_start: 0,
        query_ran: false,
        hook_fired: false,
        changes_reported: false,
        current_row: None,
    });
    stmt.conn.live_statements.fetch_add(1, Ordering::AcqRel);
    *pp_stmt = Box::into_raw(stmt) as *mut sqlite3_stmt;
    conn.clear_err();
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare_v2(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    prepare_impl(db, z_sql, n_byte, 0, pp_stmt, pz_tail)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    prepare_impl(db, z_sql, n_byte, 0, pp_stmt, pz_tail)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare_v3(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    flags: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    prepare_impl(db, z_sql, n_byte, flags, pp_stmt, pz_tail)
}

// ---------------------------------------------------------------------------
// step / finalize / reset
// ---------------------------------------------------------------------------

fn free_value_pool(stmt: &mut Stmt) {
    for p in stmt.value_pool.drain(..) {
        drop(unsafe { Box::from_raw(p as *mut Value) });
    }
}

/// Run the "Once" path: execute one statement through the mutable path,
/// applying transaction bookkeeping. Returns the result code.
fn run_once(stmt: &mut Stmt) -> c_int {
    let engine = stmt.engine.clone().expect("engine");
    let conn = stmt.conn.clone();
    let sql = match &stmt.exec {
        Exec::Once { sql } => sql.clone(),
        _ => unreachable!(),
    };

    // Transaction-control handling: BEGIN waits for the engine-level tx
    // slot (cross-connection serialization, SQLite-style BUSY).
    let ast = rustqlite::sql::parser::parse(&sql).ok();
    let is_tx = ast.as_ref().map(is_tx_control).unwrap_or(false);
    if is_tx {
        let is_begin = matches!(ast, Some(rustqlite::sql::ast::Statement::Begin(_)));
        let is_commit = matches!(ast, Some(rustqlite::sql::ast::Statement::Commit));
        let is_rollback = matches!(ast, Some(rustqlite::sql::ast::Statement::Rollback(_)));
        if is_begin {
            if conn.state.in_tx.load(Ordering::Acquire) {
                return conn.set_err(
                    SQLITE_ERROR,
                    "cannot start a transaction within a transaction",
                );
            }
            let timeout = conn.state.busy_timeout_ms.load(Ordering::Acquire);
            if !await_tx_slot(&engine, conn.id, timeout) {
                return conn.set_err(SQLITE_BUSY, "database is locked");
            }
        }
        let result = {
            let mut w = engine.db.write();
            w.execute(&sql, [])
        };
        match result {
            Ok(()) => {
                if is_begin {
                    engine.tx_owner.store(conn.id, Ordering::Release);
                    conn.state.in_tx.store(true, Ordering::Release);
                } else if is_commit || is_rollback {
                    engine.tx_owner.store(0, Ordering::Release);
                    conn.state.in_tx.store(false, Ordering::Release);
                    if is_commit {
                        fire_commit_hook(&conn);
                    } else {
                        fire_rollback_hook(&conn);
                    }
                }
                conn.clear_err();
                SQLITE_OK
            }
            Err(e) => set_conn_err(&conn, &e),
        }
    } else {
        // Non-tx statement: if another connection holds the tx slot and
        // this statement writes, wait (SQLite: BUSY during a foreign tx).
        let writes = stmt.is_write;
        if writes {
            let timeout = conn.state.busy_timeout_ms.load(Ordering::Acquire);
            if !await_tx_slot(&engine, conn.id, timeout) {
                return conn.set_err(SQLITE_BUSY, "database is locked");
            }
        }
        let before = engine.total_changes();
        let result = {
            let mut w = engine.db.write();
            w.execute(&sql, [])
        };
        match result {
            Ok(()) => {
                let after = engine.total_changes();
                let delta = after - before;
                conn.state.changes.store(delta, Ordering::Release);
                conn.state.total_changes.fetch_add(delta, Ordering::AcqRel);
                if delta > 0 {
                    conn.state
                        .last_rowid
                        .store(engine.last_rowid(), Ordering::Release);
                    fire_update_hook(&conn, &sql, engine.last_rowid());
                }
                conn.clear_err();
                SQLITE_OK
            }
            Err(e) => set_conn_err(&conn, &e),
        }
    }
}

fn fire_commit_hook(conn: &Arc<Conn>) {
    let h = conn.commit_hook.lock().unwrap();
    if let Some((ctx, Some(cb))) = h.as_ref() {
        let cb = *cb;
        let ctx = *ctx;
        drop(h);
        unsafe { cb(ctx) };
    }
}

fn fire_rollback_hook(conn: &Arc<Conn>) {
    let h = conn.rollback_hook.lock().unwrap();
    if let Some((ctx, Some(cb))) = h.as_ref() {
        let cb = *cb;
        let ctx = *ctx;
        drop(h);
        unsafe { cb(ctx) };
    }
}

/// Best-effort update hook: (op, table, rowid) with op from the statement
/// kind — fires after any DML that changed rows.
fn fire_update_hook(conn: &Arc<Conn>, sql: &str, rowid: i64) {
    let h = conn.update_hook.lock().unwrap();
    let Some((ctx, Some(cb))) = h.as_ref().copied() else {
        return;
    };
    drop(h);
    let lower = sql.trim_start().to_ascii_lowercase();
    let (op, table) = if lower.starts_with("insert") {
        (SQLITE_INSERT, table_name_of(sql))
    } else if lower.starts_with("update") {
        (SQLITE_UPDATE, table_name_of(sql))
    } else if lower.starts_with("delete") {
        (SQLITE_DELETE, table_name_of(sql))
    } else {
        return;
    };
    let table = CString::new(table).unwrap_or_default();
    let db = std::ptr::null_mut();
    unsafe { cb(ctx, op, db, table.as_ptr(), rowid) };
}

const SQLITE_INSERT: c_int = 18;
const SQLITE_DELETE: c_int = 9;
const SQLITE_UPDATE: c_int = 23;

fn table_name_of(sql: &str) -> String {
    // Grab the identifier after INSERT INTO / UPDATE / DELETE FROM.
    let toks: Vec<&str> = sql.split_whitespace().collect();
    for i in 0..toks.len().saturating_sub(1) {
        let t = toks[i].to_ascii_uppercase();
        if (t == "INTO" || t == "UPDATE" || t == "FROM")
            && (i > 0
                && (toks[i - 1].eq_ignore_ascii_case("insert")
                    || toks[i - 1].eq_ignore_ascii_case("update")
                    || toks[i - 1].eq_ignore_ascii_case("delete")))
        {
            return toks[i + 1]
                .trim_matches(|c| c == '`' || c == '"' || c == '[' || c == ']')
                .to_string();
        }
    }
    String::new()
}

/// sqlite3_step — the state machine.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int {
    let Some(s) = (stmt as *mut Stmt).as_mut() else {
        return SQLITE_MISUSE;
    };
    let conn = s.conn.clone();
    // Progress handler (approximation: checked once per step).
    {
        let ph = conn.progress.lock().unwrap();
        if ph.0 > 0 {
            if let Some(cb) = ph.1 {
                let ctx = ph.2;
                drop(ph);
                if unsafe { cb(ctx) } != 0 {
                    s.last_step_err = SQLITE_INTERRUPT;
                    return conn.set_err(SQLITE_INTERRUPT, "interrupted");
                }
            }
        }
    }
    if s.done {
        return SQLITE_DONE;
    }
    free_value_pool(s);
    s.text_buf.clear();
    s.stepped = true;
    s.total_at_step_start = s.engine.as_ref().map(|e| e.total_changes()).unwrap_or(0);

    let rc = match &mut s.exec {
        Exec::Once { .. } => {
            let rc = run_once(s);
            if rc == SQLITE_OK {
                s.done = true;
                SQLITE_DONE
            } else {
                s.last_step_err = rc;
                s.done = true;
                rc
            }
        }
        Exec::Query { sql, rows, columns } => {
            if !s.query_ran {
                // First step: execute the query now (read pragmas, EXPLAIN,
                // and other query forms the statement layer can't stream).
                let engine = s.engine.as_ref().unwrap().clone();
                let result = {
                    let rd = engine.db.read();
                    rd.query_with_columns(sql, [])
                };
                match result {
                    Ok((cols, result_rows)) => {
                        // Replace the placeholder static column set with
                        // the real one (the pragma name was provisional).
                        s.static_columns.clear();
                        for c in &cols {
                            if let Ok(cc) = CString::new(c.clone()) {
                                s.static_columns.push(cc);
                            }
                        }
                        *columns = cols;
                        *rows = result_rows.into_iter();
                        s.query_ran = true;
                    }
                    Err(e) => return finish_step_err(s, &conn, &e),
                }
            }
            match rows.next() {
                Some(row) => {
                    s.current_row = Some(row);
                    SQLITE_ROW
                }
                None => {
                    s.done = true;
                    s.current_row = None;
                    SQLITE_DONE
                }
            }
        }
        Exec::Rows(eng) => {
            // SELECT: read lock around step; DML: write lock.
            let engine = s.engine.as_ref().unwrap().clone();
            let before = engine.total_changes();
            let outcome = if s.is_write {
                let _w = engine.db.write();
                eng.step()
            } else {
                let _r = engine.db.read();
                eng.step()
            };
            let after = engine.total_changes();
            let delta = after - before;
            if delta > 0 {
                conn.state.changes.store(delta, Ordering::Release);
                conn.state.total_changes.fetch_add(delta, Ordering::AcqRel);
                conn.state
                    .last_rowid
                    .store(engine.last_rowid(), Ordering::Release);
                s.changes_reported = true;
                if !s.hook_fired {
                    fire_update_hook(&conn, &s.sql.to_string_lossy(), engine.last_rowid());
                    s.hook_fired = true;
                }
            } else if matches!(s.kind, StmtKind::Dml { .. })
                && eng_done(&outcome)
                && !s.changes_reported
            {
                // 0-row DML: SQLite reports changes() == 0 — but only
                // once per execution (a RETURNING statement's trailing
                // Done step must not clobber the real count).
                conn.state.changes.store(0, Ordering::Release);
                s.changes_reported = true;
            }
            match outcome {
                Ok(StepResult::Row) => SQLITE_ROW,
                Ok(StepResult::Done) => {
                    s.done = true;
                    SQLITE_DONE
                }
                Err(e) => {
                    s.done = true;
                    let rc = set_conn_err(&conn, &e);
                    s.last_step_err = rc;
                    return rc;
                }
            }
        }
    };
    if rc == SQLITE_ROW || rc == SQLITE_DONE {
        s.last_step_err = SQLITE_OK;
    }
    rc
}

fn eng_done(o: &Result<StepResult, rustqlite::Error>) -> bool {
    matches!(o, Ok(StepResult::Done))
}

unsafe fn finish_step_err(s: &mut Stmt, conn: &Arc<Conn>, e: &rustqlite::Error) -> c_int {
    s.done = true;
    let rc = set_conn_err(conn, e);
    s.last_step_err = rc;
    rc
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int {
    if stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let mut s = Box::from_raw(stmt as *mut Stmt);
    free_value_pool(&mut s);
    let conn = s.conn.clone();
    conn.live_statements.fetch_sub(1, Ordering::AcqRel);
    let rc = s.last_step_err;
    drop(s);
    rc
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_reset(stmt: *mut sqlite3_stmt) -> c_int {
    let Some(s) = (stmt as *mut Stmt).as_mut() else {
        return SQLITE_MISUSE;
    };
    free_value_pool(s);
    s.text_buf.clear();
    s.stepped = false;
    s.done = false;
    s.query_ran = false;
    s.hook_fired = false;
    s.changes_reported = false;
    s.current_row = None;
    match &mut s.exec {
        Exec::Rows(eng) => eng.reset(),
        Exec::Query { rows, .. } => {
            *rows = Vec::new().into_iter();
        }
        Exec::Once { .. } => {}
    }
    let rc = s.last_step_err;
    s.last_step_err = SQLITE_OK;
    rc
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_clear_bindings(stmt: *mut sqlite3_stmt) -> c_int {
    let Some(s) = (stmt as *mut Stmt).as_mut() else {
        return SQLITE_MISUSE;
    };
    if let Exec::Rows(eng) = &mut s.exec {
        eng.clear_bindings();
    }
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_stmt_readonly(stmt: *mut sqlite3_stmt) -> c_int {
    let Some(s) = (stmt as *const Stmt).as_ref() else {
        return 0;
    };
    if s.is_write {
        0
    } else {
        1
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_sql(stmt: *mut sqlite3_stmt) -> *const c_char {
    let Some(s) = (stmt as *const Stmt).as_ref() else {
        return c"".as_ptr();
    };
    s.sql.as_ptr()
}

// ---------------------------------------------------------------------------
// Parameter binding (1-based indices, like sqlite3_bind_*)
// ---------------------------------------------------------------------------

unsafe fn bind_value(stmt: *mut sqlite3_stmt, idx: c_int, v: Value) -> c_int {
    let Some(s) = (stmt as *mut Stmt).as_mut() else {
        return SQLITE_MISUSE;
    };
    let n = s.params.len() as c_int;
    if idx < 1 || idx as usize > s.params.len() {
        return s.conn.set_err(SQLITE_RANGE, "parameter index out of range");
    }
    let slot = &s.params[idx as usize - 1];
    let r = if let Exec::Rows(eng) = &mut s.exec {
        match slot {
            ParamSlot::Positional(pidx) => eng.bind(pidx + 1, v),
            ParamSlot::Named(name) => eng.bind_named(name, v),
        }
    } else {
        Ok(())
    };
    let _ = n;
    match r {
        Ok(()) => SQLITE_OK,
        Err(e) => set_conn_err(&s.conn, &e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_int64(stmt: *mut sqlite3_stmt, idx: c_int, v: i64) -> c_int {
    bind_value(stmt, idx, Value::Integer(v))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_int(stmt: *mut sqlite3_stmt, idx: c_int, v: c_int) -> c_int {
    bind_value(stmt, idx, Value::Integer(v as i64))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_double(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    v: c_double,
) -> c_int {
    bind_value(stmt, idx, Value::Real(v))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_null(stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
    bind_value(stmt, idx, Value::Null)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_text64(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_char,
    len: u64,
    _destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    _encoding: c_uchar,
) -> c_int {
    if val.is_null() {
        return bind_value(stmt, idx, Value::Null);
    }
    let len = len as usize;
    let bytes = std::slice::from_raw_parts(val as *const u8, len);
    let text = String::from_utf8_lossy(bytes).into_owned();
    bind_value(stmt, idx, Value::Text(text.into()))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_text(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_char,
    len: c_int,
    _destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    let len64 = if len < 0 {
        CStr::from_ptr(val).to_bytes().len() as u64
    } else {
        len as u64
    };
    sqlite3_bind_text64(stmt, idx, val, len64, _destructor, SQLITE_TEXT as c_uchar)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_blob64(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    len: u64,
    _destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    if val.is_null() {
        return bind_value(stmt, idx, Value::Null);
    }
    let bytes = std::slice::from_raw_parts(val as *const u8, len as usize);
    bind_value(stmt, idx, Value::Blob(bytes.to_vec()))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_blob(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const c_void,
    len: c_int,
    _destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    let len64 = if len < 0 { 0 } else { len as u64 };
    sqlite3_bind_blob64(stmt, idx, val, len64, _destructor)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_value(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    val: *const sqlite3_value,
) -> c_int {
    if val.is_null() {
        return bind_value(stmt, idx, Value::Null);
    }
    let v = &*(val as *const Value);
    bind_value(stmt, idx, v.clone())
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_zeroblob(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    n: c_int,
) -> c_int {
    bind_value(stmt, idx, Value::Blob(vec![0u8; n.max(0) as usize]))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_count(stmt: *mut sqlite3_stmt) -> c_int {
    let Some(s) = (stmt as *const Stmt).as_ref() else {
        return 0;
    };
    s.params.len() as c_int
}

/// The SQLite parameter name for a slot, or NULL for positional.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_name(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_char {
    let Some(s) = (stmt as *const Stmt).as_ref() else {
        return std::ptr::null();
    };
    if idx < 1 || idx as usize > s.params.len() {
        return std::ptr::null();
    }
    match &s.params[idx as usize - 1] {
        ParamSlot::Named(name) => name.as_ptr() as *const c_char,
        ParamSlot::Positional(_) => std::ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_index(
    stmt: *mut sqlite3_stmt,
    name: *const c_char,
) -> c_int {
    let Some(s) = (stmt as *const Stmt).as_ref() else {
        return 0;
    };
    let Some(name) = cstr(name) else { return 0 };
    for (i, slot) in s.params.iter().enumerate() {
        if let ParamSlot::Named(n) = slot {
            if n == name {
                return (i + 1) as c_int;
            }
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Column metadata + accessors
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_count(stmt: *mut sqlite3_stmt) -> c_int {
    let Some(s) = (stmt as *const Stmt).as_ref() else {
        return 0;
    };
    if let Exec::Rows(eng) = &s.exec {
        if s.static_columns.is_empty() {
            let n = eng.column_count();
            if n > 0 {
                return n as c_int;
            }
        }
    }
    s.static_columns.len() as c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_name(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_char {
    let Some(s) = (stmt as *mut Stmt).as_mut() else {
        return c"".as_ptr();
    };
    // Prefer the cached static names; fall through to the engine's
    // (driver-covered statements where we didn't snapshot them).
    if let Some(c) = s.static_columns.get(i as usize) {
        return c.as_ptr();
    }
    if let Exec::Rows(eng) = &s.exec {
        if let Some(name) = eng.column_name(i as usize) {
            // Copy into the scratch buffer (stable until next accessor).
            s.text_buf.clear();
            s.text_buf.extend_from_slice(name.as_bytes());
            s.text_buf.push(0);
            return s.text_buf.as_ptr() as *const c_char;
        }
    }
    c"".as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_decltype(
    _stmt: *mut sqlite3_stmt,
    _i: c_int,
) -> *const c_char {
    // No declared-type tracking at the C ABI layer yet; sqlx falls back to
    // the runtime column type (NULL decltype is the SQLite behavior for
    // expressions).
    std::ptr::null()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_database_name(
    _stmt: *mut sqlite3_stmt,
    _i: c_int,
) -> *const c_char {
    std::ptr::null()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_table_name(
    _stmt: *mut sqlite3_stmt,
    _i: c_int,
) -> *const c_char {
    std::ptr::null()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_origin_name(
    _stmt: *mut sqlite3_stmt,
    _i: c_int,
) -> *const c_char {
    std::ptr::null()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_metadata(
    _stmt: *mut sqlite3_stmt,
    _i: c_int,
    _pz_datatype: *mut *const c_char,
    _pz_collseq: *mut *const c_char,
    _pnotnull: *mut c_int,
    _ppk: *mut c_int,
    _pz_autoinc: *mut *const c_char,
) -> c_int {
    SQLITE_ERROR
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_table_column_metadata(
    db: *mut sqlite3,
    _db_name: *const c_char,
    table_name: *const c_char,
    column_name: *const c_char,
    pz_datatype: *mut *const c_char,
    pz_collseq: *mut *const c_char,
    pnotnull: *mut c_int,
    ppk: *mut c_int,
    pz_autoinc: *mut *const c_char,
) -> c_int {
    let conn: &Conn = match (db as *const Conn).as_ref() {
        Some(c) => c,
        None => return SQLITE_MISUSE,
    };
    let Some(engine) = &conn.engine else {
        return SQLITE_MISUSE;
    };
    let Some(table) = cstr(table_name) else {
        return SQLITE_MISUSE;
    };
    let Some(column) = cstr(column_name) else {
        return SQLITE_MISUSE;
    };
    let rd = engine.db.read();
    let meta = rd.table_column_metadata(table, column);
    match meta {
        Some((decl_type, not_null, pk)) => {
            let dt = static_decltype(&decl_type);
            if !pz_datatype.is_null() {
                *pz_datatype = dt;
            }
            if !pz_collseq.is_null() {
                *pz_collseq = c"BINARY".as_ptr();
            }
            if !pnotnull.is_null() {
                *pnotnull = if not_null { 1 } else { 0 };
            }
            if !ppk.is_null() {
                *ppk = if pk { 1 } else { 0 };
            }
            if !pz_autoinc.is_null() {
                *pz_autoinc = std::ptr::null();
            }
            SQLITE_OK
        }
        None => conn.set_err(SQLITE_ERROR, "no such table column"),
    }
}

fn static_decltype(t: &str) -> *const c_char {
    static TABLE: OnceLock<StdMutex<HashMap<&'static str, CString>>> = OnceLock::new();
    let m = TABLE.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut m = m.lock().unwrap();
    if let Some(c) = m.get(t) {
        return c.as_ptr();
    }
    // Leak a lowercase-normalized copy, keyed by a leaked &str.
    let key: &'static str = Box::leak(t.to_string().into_boxed_str());
    let c = CString::new(t.to_string()).unwrap();
    let p = c.as_ptr();
    m.insert(key, c);
    p
}

/// Current row's value at column i (unified across Exec kinds).
fn stmt_value(s: &Stmt, i: usize) -> Option<Value> {
    match &s.exec {
        Exec::Rows(eng) => eng.column_value(i).cloned(),
        Exec::Query { .. } => s.current_row.as_ref()?.get(i).cloned(),
        Exec::Once { .. } => None,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_type(stmt: *mut sqlite3_stmt, i: c_int) -> c_int {
    let Some(s) = (stmt as *const Stmt).as_ref() else {
        return SQLITE_NULL;
    };
    match stmt_value(s, i as usize) {
        Some(Value::Null) | None => SQLITE_NULL,
        Some(Value::Integer(_)) => SQLITE_INTEGER,
        Some(Value::Real(_)) => SQLITE_FLOAT,
        Some(Value::Text(_)) => SQLITE_TEXT,
        Some(Value::Blob(_)) => SQLITE_BLOB,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_int64(stmt: *mut sqlite3_stmt, i: c_int) -> i64 {
    let Some(s) = (stmt as *const Stmt).as_ref() else {
        return 0;
    };
    stmt_value(s, i as usize)
        .map(|v| v.as_integer())
        .unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_int(stmt: *mut sqlite3_stmt, i: c_int) -> c_int {
    sqlite3_column_int64(stmt, i) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_double(stmt: *mut sqlite3_stmt, i: c_int) -> f64 {
    let Some(s) = (stmt as *const Stmt).as_ref() else {
        return 0.0;
    };
    stmt_value(s, i as usize)
        .map(|v| v.as_real())
        .unwrap_or(0.0)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_blob(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_void {
    let Some(s) = (stmt as *const Stmt).as_ref() else {
        return std::ptr::null();
    };
    match stmt_value(s, i as usize) {
        Some(Value::Blob(b)) if !b.is_empty() => b.as_ptr() as *const c_void,
        _ => std::ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_bytes(stmt: *mut sqlite3_stmt, i: c_int) -> c_int {
    let Some(s) = (stmt as *const Stmt).as_ref() else {
        return 0;
    };
    match stmt_value(s, i as usize) {
        Some(Value::Blob(b)) => b.len() as c_int,
        Some(Value::Text(t)) => t.as_bytes().len() as c_int,
        _ => 0,
    }
}

/// NUL-terminated text in a per-statement scratch buffer (valid until the
/// next column_text / value_text call on this statement).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_text(stmt: *mut sqlite3_stmt, i: c_int) -> *const u_uchar {
    let Some(s) = (stmt as *mut Stmt).as_mut() else {
        return std::ptr::null();
    };
    let text = match stmt_value(s, i as usize) {
        Some(Value::Text(t)) => t.as_str().to_string(),
        Some(Value::Integer(n)) => n.to_string(),
        Some(Value::Real(f)) => f.to_string(),
        _ => return std::ptr::null(),
    };
    s.text_buf.clear();
    s.text_buf.extend_from_slice(text.as_bytes());
    s.text_buf.push(0);
    s.text_buf.as_ptr() as *const u_uchar
}

/// Allocate an unprotected value object for the current row (freed at the
/// next step / reset / finalize — the SQLite lifetime contract).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_value(
    stmt: *mut sqlite3_stmt,
    i: c_int,
) -> *mut sqlite3_value {
    let Some(s) = (stmt as *mut Stmt).as_mut() else {
        return std::ptr::null_mut();
    };
    match stmt_value(s, i as usize) {
        Some(v) => {
            let boxed = Box::new(v);
            let p = Box::into_raw(boxed) as *mut sqlite3_value;
            s.value_pool.push(p);
            p
        }
        None => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
// sqlite3_value accessors (value objects are Box<Value>)
// ---------------------------------------------------------------------------

unsafe fn value_of(v: *const sqlite3_value) -> Option<&'static Value> {
    if v.is_null() {
        return None;
    }
    (v as *const Value).as_ref()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_type(v: *const sqlite3_value) -> c_int {
    match value_of(v) {
        Some(Value::Null) | None => SQLITE_NULL,
        Some(Value::Integer(_)) => SQLITE_INTEGER,
        Some(Value::Real(_)) => SQLITE_FLOAT,
        Some(Value::Text(_)) => SQLITE_TEXT,
        Some(Value::Blob(_)) => SQLITE_BLOB,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_int64(v: *const sqlite3_value) -> i64 {
    value_of(v).map(|x| x.as_integer()).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_int(v: *const sqlite3_value) -> c_int {
    sqlite3_value_int64(v) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_double(v: *const sqlite3_value) -> c_double {
    value_of(v).map(|x| x.as_real()).unwrap_or(0.0)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_blob(v: *const sqlite3_value) -> *const c_void {
    match value_of(v) {
        Some(Value::Blob(b)) if !b.is_empty() => b.as_ptr() as *const c_void,
        _ => std::ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_bytes(v: *const sqlite3_value) -> c_int {
    match value_of(v) {
        Some(Value::Blob(b)) => b.len() as c_int,
        Some(Value::Text(t)) => t.as_bytes().len() as c_int,
        _ => 0,
    }
}

/// NUL-terminated text in a thread-local scratch buffer (valid until the
/// next value_text call).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_text(v: *const sqlite3_value) -> *const u_uchar {
    thread_local! {
        static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let text = match value_of(v) {
        Some(Value::Text(t)) => t.as_str().to_string(),
        Some(Value::Integer(n)) => n.to_string(),
        Some(Value::Real(f)) => f.to_string(),
        _ => return std::ptr::null(),
    };
    SCRATCH.with(|s| {
        let mut b = s.borrow_mut();
        b.clear();
        b.extend_from_slice(text.as_bytes());
        b.push(0);
        b.as_ptr()
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_dup(v: *const sqlite3_value) -> *mut sqlite3_value {
    match value_of(v) {
        Some(val) => Box::into_raw(Box::new(val.clone())) as *mut sqlite3_value,
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_free(v: *mut sqlite3_value) {
    if !v.is_null() {
        drop(unsafe { Box::from_raw(v as *mut Value) });
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_numeric_type(v: *const sqlite3_value) -> c_int {
    sqlite3_value_type(v)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_frombind(_v: *const sqlite3_value) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_nochange(_v: *const sqlite3_value) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_pointer(
    v: *const sqlite3_value,
    _name: *const c_char,
) -> *mut c_void {
    let _ = v;
    std::ptr::null_mut()
}

// ---------------------------------------------------------------------------
// Function-call context (sqlite3_context) — result setters
// ---------------------------------------------------------------------------

// The plugin ABI owns the CallCtx representation; re-export its accessors.
use rustqlite::plugin::abi as plugin_abi;

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_int(_ctx: *mut sqlite3_context, v: c_int) {
    plugin_abi::api_result_int(_ctx as *mut plugin_abi::RqlContext, v);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_int64(_ctx: *mut sqlite3_context, v: i64) {
    plugin_abi::api_result_int64(_ctx as *mut plugin_abi::RqlContext, v);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_double(_ctx: *mut plugin_abi::RqlContext, v: c_double) {
    plugin_abi::api_result_double(_ctx, v);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_null(_ctx: *mut sqlite3_context) {
    plugin_abi::api_result_null(_ctx as *mut plugin_abi::RqlContext);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_text(
    ctx: *mut sqlite3_context,
    s: *const c_char,
    len: c_int,
) {
    plugin_abi::api_result_text(ctx as *mut plugin_abi::RqlContext, s, len);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_text64(
    ctx: *mut sqlite3_context,
    s: *const c_char,
    len: u64,
    _d: Option<unsafe extern "C" fn(*mut c_void)>,
    _enc: c_uchar,
) {
    plugin_abi::api_result_text(ctx as *mut plugin_abi::RqlContext, s, len as c_int);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_blob(
    ctx: *mut sqlite3_context,
    data: *const c_void,
    len: c_int,
) {
    plugin_abi::api_result_blob(ctx as *mut plugin_abi::RqlContext, data, len);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error(
    ctx: *mut sqlite3_context,
    msg: *const c_char,
    len: c_int,
) {
    plugin_abi::api_result_error(ctx as *mut plugin_abi::RqlContext, msg, len);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error_code(_ctx: *mut sqlite3_context, _code: c_int) {
    // The engine's error surface is message-based; the code is recorded
    // best-effort (sqlx only reads it via the error path).
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_user_data(_ctx: *mut sqlite3_context) -> *mut c_void {
    plugin_abi::api_user_data(_ctx as *mut plugin_abi::RqlContext)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_aggregate_context(
    ctx: *mut sqlite3_context,
    n: c_int,
) -> *mut c_void {
    plugin_abi::api_aggregate_context(ctx as *mut plugin_abi::RqlContext, n)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_context_db_handle(_ctx: *mut sqlite3_context) -> *mut sqlite3 {
    std::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_get_auxdata(_ctx: *mut sqlite3_context, _n: c_int) -> *mut c_void {
    // No aux-data caching yet (regexp recompiles per call — correct, slower).
    std::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_set_auxdata(
    _ctx: *mut sqlite3_context,
    _n: c_int,
    _p: *mut c_void,
    _destroy: Option<unsafe extern "C" fn(*mut c_void)>,
) {
}

// ---------------------------------------------------------------------------
// Function / collation registration (routed to the engine plugin registry)
// ---------------------------------------------------------------------------

use rustqlite::plugin::{CAggregate, CCollation, CScalar};

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_function_v2(
    db: *mut sqlite3,
    z_name: *const c_char,
    n_arg: c_int,
    _e_text_rep: c_int,
    p_app: *mut c_void,
    x_func: Option<unsafe extern "C" fn(*mut sqlite3_context, c_int, *mut *mut sqlite3_value)>,
    x_step: Option<unsafe extern "C" fn(*mut sqlite3_context, c_int, *mut *mut sqlite3_value)>,
    x_final: Option<unsafe extern "C" fn(*mut sqlite3_context)>,
    _x_destroy: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    let conn: &Conn = match (db as *const Conn).as_ref() {
        Some(c) => c,
        None => return SQLITE_MISUSE,
    };
    let Some(engine) = &conn.engine else {
        return SQLITE_MISUSE;
    };
    let Some(name) = cstr(z_name) else {
        return conn.set_err(SQLITE_MISUSE, "invalid function name");
    };
    // The engine's plugin trampolines call back with RqlContext /
    // RqlValue — identical layouts to sqlite3_context / sqlite3_value.
    let xf = x_func.map(|f| unsafe {
        std::mem::transmute::<
            unsafe extern "C" fn(*mut sqlite3_context, c_int, *mut *mut sqlite3_value),
            unsafe extern "C" fn(
                *mut plugin_abi::RqlContext,
                c_int,
                *mut *mut plugin_abi::RqlValue,
            ),
        >(f)
    });
    let xs = x_step.map(|f| unsafe {
        std::mem::transmute::<
            unsafe extern "C" fn(*mut sqlite3_context, c_int, *mut *mut sqlite3_value),
            unsafe extern "C" fn(
                *mut plugin_abi::RqlContext,
                c_int,
                *mut *mut plugin_abi::RqlValue,
            ),
        >(f)
    });
    let xfin = x_final; // same signature
    let res = {
        let mut w = engine.db.write();
        if let Some(xf) = xf {
            w.create_function_arc(Arc::new(CScalar {
                name: name.to_string(),
                n_arg,
                app: p_app,
                x_func: xf,
            }))
        } else if let (Some(xs), Some(xfin)) = (xs, xfin) {
            let xfin2 = unsafe {
                std::mem::transmute::<
                    unsafe extern "C" fn(*mut sqlite3_context),
                    unsafe extern "C" fn(*mut plugin_abi::RqlContext),
                >(xfin)
            };
            w.create_aggregate_arc(Arc::new(CAggregate {
                name: name.to_string(),
                n_arg,
                app: p_app,
                x_step: xs,
                x_final: xfin2,
            }))
        } else {
            Err(rustqlite::Error::semantic(
                "create_function requires xFunc or xStep+xFinal",
            ))
        }
    };
    match res {
        Ok(()) => SQLITE_OK,
        Err(e) => set_conn_err(conn, &e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_function(
    db: *mut sqlite3,
    z_name: *const c_char,
    n_arg: c_int,
    e_text_rep: c_int,
    p_app: *mut c_void,
    x_func: Option<unsafe extern "C" fn(*mut sqlite3_context, c_int, *mut *mut sqlite3_value)>,
    x_step: Option<unsafe extern "C" fn(*mut sqlite3_context, c_int, *mut *mut sqlite3_value)>,
    x_final: Option<unsafe extern "C" fn(*mut sqlite3_context)>,
) -> c_int {
    sqlite3_create_function_v2(
        db, z_name, n_arg, e_text_rep, p_app, x_func, x_step, x_final, None,
    )
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_collation(
    db: *mut sqlite3,
    z_name: *const c_char,
    _e_text_rep: c_int,
    p_arg: *mut c_void,
    cmp: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
    >,
) -> c_int {
    let conn: &Conn = match (db as *const Conn).as_ref() {
        Some(c) => c,
        None => return SQLITE_MISUSE,
    };
    let Some(engine) = &conn.engine else {
        return SQLITE_MISUSE;
    };
    let Some(name) = cstr(z_name) else {
        return conn.set_err(SQLITE_MISUSE, "invalid collation name");
    };
    let Some(xc) = cmp else {
        return SQLITE_OK; // NULL unregisters
    };
    let res = {
        let mut w = engine.db.write();
        w.create_collation_arc(Arc::new(CCollation {
            name: name.to_string(),
            app: p_arg,
            x_compare: xc,
        }))
    };
    match res {
        Ok(()) => SQLITE_OK,
        Err(e) => set_conn_err(conn, &e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_collation_v2(
    db: *mut sqlite3,
    z_name: *const c_char,
    e_text_rep: c_int,
    p_arg: *mut c_void,
    cmp: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
    >,
    _x_destroy: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    sqlite3_create_collation(db, z_name, e_text_rep, p_arg, cmp)
}

// ---------------------------------------------------------------------------
// Hooks / progress handler / interrupt
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3_progress_handler(
    db: *mut sqlite3,
    n: c_int,
    cb: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    ctx: *mut c_void,
) {
    let Some(conn) = (db as *const Conn).as_ref() else {
        return;
    };
    *conn.progress.lock().unwrap() = (n, cb, ctx);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_update_hook(
    db: *mut sqlite3,
    cb: Option<unsafe extern "C" fn(*mut c_void, c_int, *const c_char, *const c_char, i64)>,
    ctx: *mut c_void,
) -> *mut c_void {
    let Some(conn) = (db as *const Conn).as_ref() else {
        return std::ptr::null_mut();
    };
    let mut h = conn.update_hook.lock().unwrap();
    let old = h.replace((ctx, cb)).map(|(_, _)| ()).and(Some(0));
    old.map_or(std::ptr::null_mut(), |_| std::ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_commit_hook(
    db: *mut sqlite3,
    cb: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    ctx: *mut c_void,
) -> *mut c_void {
    let Some(conn) = (db as *const Conn).as_ref() else {
        return std::ptr::null_mut();
    };
    let mut h = conn.commit_hook.lock().unwrap();
    let old = h.replace((ctx, cb)).map(|(c, _)| c);
    old.unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_rollback_hook(
    db: *mut sqlite3,
    cb: Option<unsafe extern "C" fn(*mut c_void)>,
    ctx: *mut c_void,
) -> *mut c_void {
    let Some(conn) = (db as *const Conn).as_ref() else {
        return std::ptr::null_mut();
    };
    let mut h = conn.rollback_hook.lock().unwrap();
    let old = h.replace((ctx, cb)).map(|(c, _)| c);
    old.unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_interrupt(_db: *mut sqlite3) {
    // Interrupt is asynchronous in SQLite; the progress-handler path
    // covers sqlx's cancellation usage.
}

// ---------------------------------------------------------------------------
// exec
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3_exec(
    db: *mut sqlite3,
    sql: *const c_char,
    _callback: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
    >,
    _arg: *mut c_void,
    _errmsg: *mut *mut c_char,
) -> c_int {
    let conn: &Conn = match (db as *const Conn).as_ref() {
        Some(c) => c,
        None => return SQLITE_MISUSE,
    };
    if conn.engine.is_none() {
        return conn.set_err(SQLITE_MISUSE, "connection is in error state");
    }
    let Some(sql) = cstr(sql) else {
        return conn.set_err(SQLITE_MISUSE, "invalid SQL text");
    };
    // Run each statement in the script through the prepare+step machinery
    // (reuses transaction and busy handling).
    let mut remaining = sql.to_string();
    let mut rc;
    while !remaining.trim().is_empty() {
        let mut stmt: *mut sqlite3_stmt = std::ptr::null_mut();
        let mut tail: *const c_char = std::ptr::null();
        let c_sql = CString::new(remaining).unwrap_or_default();
        rc = sqlite3_prepare_v2(db, c_sql.as_ptr(), -1, &mut stmt, &mut tail);
        if rc != SQLITE_OK {
            return rc;
        }
        if !stmt.is_null() {
            loop {
                rc = sqlite3_step(stmt);
                if rc != SQLITE_ROW {
                    break;
                }
            }
            if rc != SQLITE_DONE {
                sqlite3_finalize(stmt);
                return rc;
            }
            rc = sqlite3_finalize(stmt);
            if rc != SQLITE_OK {
                return rc;
            }
        }
        let consumed = tail as usize - c_sql.as_ptr() as usize;
        remaining = c_sql.to_string_lossy()[consumed..].to_string();
    }
    conn.clear_err();
    SQLITE_OK
}

// ---------------------------------------------------------------------------
// Extension loading
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3_load_extension(
    db: *mut sqlite3,
    path: *const c_char,
    entry: *const c_char,
    _err: *mut *mut c_char,
) -> c_int {
    let conn: &Conn = match (db as *const Conn).as_ref() {
        Some(c) => c,
        None => return SQLITE_MISUSE,
    };
    let Some(engine) = &conn.engine else {
        return SQLITE_MISUSE;
    };
    let Some(p) = cstr(path) else {
        return conn.set_err(SQLITE_MISUSE, "invalid extension path");
    };
    let entry = if entry.is_null() { None } else { cstr(entry) };
    let res = {
        let mut w = engine.db.write();
        w.load_extension(std::path::Path::new(p), entry)
    };
    match res {
        Ok(()) => SQLITE_OK,
        Err(e) => set_conn_err(conn, &e),
    }
}

/// C-variadic: only SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION matters to sqlx.
/// Extension loading is always permitted (opt-in feature), so the args are
/// not read — the value is accepted regardless.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_config(
    db: *mut sqlite3,
    op: c_int,
    _arg1: c_int,
    _arg2: *mut c_int,
) -> c_int {
    // Fixed-arity bridge: the only caller in the sqlx tree passes exactly
    // (db, SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION, flag, NULL). Extension
    // loading is always permitted here, so the varargs are not read.
    let _ = db;
    match op {
        SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION => SQLITE_OK,
        _ => SQLITE_ERROR,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_enable_load_extension(_db: *mut sqlite3, _on: c_int) -> c_int {
    SQLITE_OK
}

// ---------------------------------------------------------------------------
// unlock-notify / preupdate / serialize stubs (link-complete, minimal)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3_unlock_notify(
    _db: *mut sqlite3,
    x_notify: Option<unsafe extern "C" fn(*mut *mut c_void, c_int)>,
    p_arg: *mut c_void,
) -> c_int {
    // The engine never blocks a reader behind shared-cache locks, so
    // notify immediately (the condition is already satisfied).
    if let Some(cb) = x_notify {
        let mut arg = p_arg;
        cb(&mut arg as *mut *mut c_void, 1);
    }
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_preupdate_hook(
    _db: *mut sqlite3,
    _cb: Option<
        unsafe extern "C" fn(*mut c_void, *mut sqlite3, c_int, *const c_char, *const c_char, i64),
    >,
    _ctx: *mut c_void,
) {
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_preupdate_count(_stmt: *mut sqlite3_stmt) -> c_int {
    0
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_preupdate_depth(_stmt: *mut sqlite3_stmt) -> c_int {
    0
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_preupdate_old(
    _stmt: *mut sqlite3_stmt,
    _i: c_int,
    _pp: *mut *mut sqlite3_value,
) -> c_int {
    SQLITE_ERROR
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_preupdate_new(
    _stmt: *mut sqlite3_stmt,
    _i: c_int,
    _pp: *mut *mut sqlite3_value,
) -> c_int {
    SQLITE_ERROR
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_serialize(
    _db: *mut sqlite3,
    _schema: *const c_char,
    _size: *mut i64,
    _flags: c_int,
) -> *mut c_uchar {
    std::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_deserialize(
    _db: *mut sqlite3,
    _schema: *const c_char,
    _data: *mut u_uchar,
    _db_size: i64,
    _sz: i64,
    _flags: c_int,
) -> c_int {
    SQLITE_ERROR
}

// ---------------------------------------------------------------------------
// Memory (libc-backed)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3_malloc(n: c_int) -> *mut c_void {
    let n = n.max(0) as usize;
    let mut v = Vec::<u8>::with_capacity(n.max(1));
    let p = v.as_mut_ptr() as *mut c_void;
    std::mem::forget(v);
    p
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_malloc64(n: u64) -> *mut c_void {
    let mut v = Vec::<u8>::with_capacity((n as usize).max(1));
    let p = v.as_mut_ptr() as *mut c_void;
    std::mem::forget(v);
    p
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_realloc(p: *mut c_void, n: c_int) -> *mut c_void {
    if p.is_null() {
        return sqlite3_malloc(n);
    }
    let old = Vec::from_raw_parts(p as *mut u8, 0, 0);
    drop(old);
    sqlite3_malloc(n.max(0))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_free(p: *mut c_void) {
    if !p.is_null() {
        drop(unsafe { Vec::from_raw_parts(p as *mut u8, 0, 0) });
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_memory_used() -> i64 {
    0
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_memory_alarm(
    _cb: Option<unsafe extern "C" fn(*mut c_void, c_int, i64)>,
    _ctx: *mut c_void,
    _threshold: c_int,
) -> c_int {
    SQLITE_OK
}

// A few lifecycle no-ops some tools call.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_initialize() -> c_int {
    SQLITE_OK
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_shutdown() -> c_int {
    SQLITE_OK
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_os_init() -> c_int {
    SQLITE_OK
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_os_end() -> c_int {
    SQLITE_OK
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_config(_op: c_int, _arg: *mut c_void) -> c_int {
    SQLITE_OK
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_mutex_alloc(_id: c_int) -> *mut c_void {
    std::ptr::null_mut()
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_mutex_free(_m: *mut c_void) {}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_mutex_enter(_m: *mut c_void) {}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_mutex_leave(_m: *mut c_void) {}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_mutex_try(_m: *mut c_void) -> c_int {
    SQLITE_OK
}
