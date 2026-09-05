//! # Native sqlx driver for rustqlite (`sqlx` feature)
//!
//! This module makes rustqlite work with [sqlx](https://crates.io/crates/sqlx)
//! **directly as a Rust library** — no `libsqlite3.so`, no C ABI, no
//! `staticlib`/`cdylib` build. It implements sqlx-core's `Database` driver
//! traits against the engine, so all of sqlx's generic machinery —
//! [`Pool`], transactions, `query()` / `query_as()` / `query_scalar()`,
//! `FromRow`, statement logging, timeouts — works out of the box.
//!
//! ## Why a native driver instead of the C ABI
//!
//! * **No FFI, no unsafe**: the C-ABI route (see `compat/`) requires
//!   emulating the entire SQLite C API surface and lifetime-erasing
//!   statement handles. This driver is 100% safe Rust.
//! * **Faster**: sqlx's SQLite driver runs each connection on a dedicated
//!   worker thread and ferries every command and row across channels, plus
//!   per-call FFI marshalling. This driver executes inline in the async
//!   task — for an in-memory engine that removes the dominant overhead.
//! * **Single dependency**: add `rustqlite = { features = ["sqlx"] }` and
//!   you're done — no `RUSTQLITE_LIB_DIR`, no `[patch.crates-io]`, no C
//!   toolchain, trivial cross-compilation.
//!
//! ## Usage
//!
//! ```no_run
//! # async fn demo() -> sqlx::Result<()> {
//! use rustqlite::sqlx_driver::{RustqlitePool, RustqliteConnectOptions};
//!
//! // In-memory database:
//! let opts = RustqliteConnectOptions::new();
//! // ...or a file: RustqliteConnectOptions::filename("app.db").create_if_missing(true)
//! let pool = RustqlitePool::connect_with(opts).await?;
//!
//! sqlx::query("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
//!     .execute(&pool).await?;
//!
//! let id: i64 = sqlx::query_scalar("INSERT INTO users (name) VALUES (?) RETURNING id")
//!     .bind("Ada")
//!     .fetch_one(&pool).await?;
//!
//! let name: Option<String> = sqlx::query_scalar("SELECT name FROM users WHERE id = ?")
//!     .bind(id)
//!     .fetch_optional(&pool).await?
//!     .flatten();
//! # Ok(())
//! # }
//! ```
//!
//! Both `sqlx::query(...)` (from the `sqlx` facade) and
//! `rustqlite::sqlx_driver::query(...)` (re-exported from sqlx-core) work
//! with this driver — they are the same generic functions.
//!
//! ## URL format
//!
//! * `rustqlite::memory:` / `rustqlite://:memory:` — private in-memory
//!   database (one per connection; use `max_connections(1)` or
//!   `?cache=shared` for pools).
//! * `rustqlite://:memory:?cache=shared` — process-wide shared in-memory
//!   database: every pool connection works on the same engine.
//! * `rustqlite://app.db` / `rustqlite://./app.db?mode=rwc` — file database
//!   (shared engine per canonical path, SQLite file semantics).
//!
//! ## Concurrency model
//!
//! One engine instance is created per database (canonical file path, or the
//! shared in-memory key); every pool connection wraps it in an
//! `Arc<RwLock<_>>`. Reads run concurrently under the read lock; writes take
//! the write lock for the duration of one statement.
//!
//! **Isolation (no dirty reads):** while a transaction with uncommitted
//! writes is open on one connection, other connections' reads WAIT (SQLite
//! snapshot semantics) instead of observing half-applied data. Read-only
//! transactions never block readers, so `begin()` + SELECTs stays fully
//! concurrent with other readers. Readers and writers blocked by a foreign
//! transaction wait up to the busy timeout (default 5 s, sqlx-sqlite parity
//! — see [`RustqliteConnectOptions::busy_timeout`]) and then fail with
//! `SQLITE_BUSY` ("database is locked"). A dropped connection automatically
//! rolls back whatever engine-level transaction it left open (sqlx-managed
//! or raw-script `BEGIN`), so one connection can never wedge the others.
//!
//! ## Differences from sqlx-sqlite
//!
//! * The compile-time checked macros (`query!` / `query_as!`) are hardcoded
//!   to sqlx's built-in backends in sqlx 0.9; use the runtime API (which is
//!   what sea-orm itself uses). `#[derive(sqlx::FromRow)]` does work.
//! * Statements are cached inside the engine (shared across connections by
//!   SQL text), so there is no per-connection sqlx statement cache
//!   (`HasStatementCache` is not implemented).
//! * Long queries execute inline in the async task (no worker thread). For
//!   the in-memory OLTP/OLAP shapes this engine targets, that's a latency
//!   win; a single very long query will occupy its task's thread.
//! * `mode=ro` is accepted but advisory (the engine has no read-only open
//!   yet; same as the C-ABI compat layer).

use std::collections::HashMap as StdHashMap;
use std::future::Future;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use futures_util::{future, stream, FutureExt, StreamExt};
use log::LevelFilter;

use sqlx_core::connection::{
    ConnectOptions as SqlxConnectOptions, Connection as SqlxConnection, LogSettings,
};
use sqlx_core::database::Database as SqlxDatabase;
use sqlx_core::error::Error as SqlxError;
use sqlx_core::executor::{Execute, Executor as SqlxExecutor};
use sqlx_core::logger::QueryLogger;
use sqlx_core::transaction::{
    begin_ansi_transaction_sql, commit_ansi_transaction_sql, rollback_ansi_transaction_sql,
    TransactionManager as SqlxTransactionManager,
};

pub use sqlx_core;

mod error;
mod gate;
mod statement;
mod types;

pub use error::RustqliteError;
pub use statement::{RustqliteColumn, RustqliteQueryResult, RustqliteRow, RustqliteStatement};
pub use types::{
    DataType, RustqliteArguments, RustqliteTypeInfo, RustqliteValue, RustqliteValueRef,
};

// Re-export the generic sqlx-core API so this driver can be used without
// depending on the `sqlx` facade crate.
pub use sqlx_core::acquire::Acquire;
pub use sqlx_core::column::ColumnIndex;
pub use sqlx_core::connection::{ConnectOptions, Connection};
pub use sqlx_core::error::Error;
pub use sqlx_core::executor::Executor;
pub use sqlx_core::from_row::FromRow;
pub use sqlx_core::pool::{Pool, PoolConnection, PoolOptions};
pub use sqlx_core::query::{query, query_with};
pub use sqlx_core::query_as::query_as;
pub use sqlx_core::query_scalar::{query_scalar, query_scalar_with};
pub use sqlx_core::raw_sql::raw_sql;
pub use sqlx_core::sql_str::{AssertSqlSafe, SqlStr};
pub use sqlx_core::statement::Statement;
pub use sqlx_core::transaction::Transaction;

use sqlx_core::Either;

use crate::api::Database as Engine;
use crate::sql::ast::Statement as Ast;
use crate::sql::parser;
use crate::statement::Statement as EngineStatement;
use crate::statement::StepResult;
use crate::types::Value as EngineValue;

use statement::columns_from_names;
use statement::Columns;
use statement::NameMap;

/// An alias for [`Pool`], specialized for rustqlite.
pub type RustqlitePool = Pool<Rustqlite>;
/// An alias for [`PoolOptions`], specialized for rustqlite.
pub type RustqlitePoolOptions = PoolOptions<Rustqlite>;
/// An alias for [`PoolConnection`], specialized for rustqlite.
pub type RustqlitePoolConnection = PoolConnection<Rustqlite>;
/// An alias for [`Transaction`], specialized for rustqlite.
pub type RustqliteTransaction<'c> = Transaction<'c, Rustqlite>;

// ---------------------------------------------------------------------------
// Database marker
// ---------------------------------------------------------------------------

/// Rustqlite database driver marker for sqlx.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rustqlite;

impl SqlxDatabase for Rustqlite {
    type Connection = RustqliteConnection;
    type TransactionManager = RustqliteTransactionManager;
    type Row = RustqliteRow;
    type QueryResult = RustqliteQueryResult;
    type Column = RustqliteColumn;
    type TypeInfo = RustqliteTypeInfo;
    type Value = RustqliteValue;
    type ValueRef<'r> = RustqliteValueRef<'r>;
    type Arguments = RustqliteArguments;
    type ArgumentBuffer = Vec<EngineValue>;
    type Statement = RustqliteStatement;

    const NAME: &'static str = "Rustqlite";
    const URL_SCHEMES: &'static [&'static str] = &["rustqlite"];
}

// ---------------------------------------------------------------------------
// Shared engine registry
// ---------------------------------------------------------------------------

/// One engine instance per database, shared across pool connections.
struct SharedDb {
    db: parking_lot::RwLock<Engine>,
    /// Connection id that currently owns the engine-level transaction
    /// (0 = none). Serializes cross-connection transactions with
    /// SQLite-style BUSY semantics.
    tx_owner: AtomicUsize,
    /// True once the owning transaction has executed any write (DML/DDL).
    /// Readers wait while a *dirty* foreign transaction is open so they
    /// can never observe uncommitted rows — SQLite isolation semantics.
    /// A read-only foreign transaction does not block readers.
    tx_dirty: AtomicBool,
    /// Gate + condvar for connections waiting on the open transaction.
    /// Notified whenever the transaction state changes (BEGIN/COMMIT/
    /// ROLLBACK/close). Never held while the RwLock is held, so there is
    /// no lock-ordering cycle and no deadlock is possible.
    tx_gate: parking_lot::Mutex<()>,
    tx_cv: parking_lot::Condvar,
}

impl SharedDb {
    /// True when a transaction owned by a DIFFERENT connection is open.
    fn foreign_tx(&self, me: usize) -> bool {
        let owner = self.tx_owner.load(Ordering::Acquire);
        owner != 0 && owner != me
    }

    /// Wait until no transaction owned by another connection is open.
    ///
    /// * `only_dirty` — readers pass `true`: a foreign transaction that
    ///   has not written anything yet cannot expose uncommitted state,
    ///   so reading alongside it is safe (and keeps read-only
    ///   transactions fully concurrent, like SQLite WAL snapshots).
    /// * Returns `false` on timeout (caller maps to SQLITE_BUSY).
    fn wait_tx_clear(&self, me: usize, only_dirty: bool, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if !self.foreign_tx_blocked(me, only_dirty) {
                return true;
            }
            let mut guard = self.tx_gate.lock();
            // Re-check under the gate mutex: the tx may have ended while
            // we were acquiring it.
            if !self.foreign_tx_blocked(me, only_dirty) {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            self.tx_cv.wait_for(&mut guard, deadline - now);
            drop(guard);
        }
    }

    /// Would a foreign transaction block `me` right now?
    fn foreign_tx_blocked(&self, me: usize, only_dirty: bool) -> bool {
        let owner = self.tx_owner.load(Ordering::Acquire);
        if owner == 0 || owner == me {
            return false;
        }
        if only_dirty {
            self.tx_dirty.load(Ordering::Acquire)
        } else {
            true
        }
    }

    /// Wake every connection waiting on the transaction gate — both sync
    /// waiters (our condvar) and async waiters (the gate thread registry).
    fn notify_tx_change(&self) {
        let _guard = self.tx_gate.lock();
        self.tx_cv.notify_all();
        crate::sqlx_driver::gate::gate_notify();
    }

    /// Reset transaction bookkeeping (BEGIN/COMMIT/ROLLBACK/close) and
    /// wake waiters.
    fn tx_reset(&self) {
        self.tx_owner.store(0, Ordering::Release);
        self.tx_dirty.store(false, Ordering::Release);
        self.notify_tx_change();
    }
}

fn new_shared(db: Engine) -> Arc<SharedDb> {
    Arc::new(SharedDb {
        db: parking_lot::RwLock::new(db),
        tx_owner: AtomicUsize::new(0),
        tx_dirty: AtomicBool::new(false),
        tx_gate: parking_lot::Mutex::new(()),
        tx_cv: parking_lot::Condvar::new(),
    })
}

fn engines() -> &'static StdMutex<StdHashMap<String, Arc<SharedDb>>> {
    static ENGINES: OnceLock<StdMutex<StdHashMap<String, Arc<SharedDb>>>> = OnceLock::new();
    ENGINES.get_or_init(|| StdMutex::new(StdHashMap::new()))
}

fn next_conn_id() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// A connection to a rustqlite database.
///
/// Wraps the shared engine (one per database path) behind an
/// `Arc<RwLock<...>>`; queries execute inline — there is no worker thread.
pub struct RustqliteConnection {
    shared: Arc<SharedDb>,
    id: usize,
    /// sqlx-managed transaction depth (0 = autocommit; >1 = savepoints).
    tx_depth: usize,
    /// A sqlx-managed DEFERRED transaction is open (the engine-level
    /// transaction may not have started yet — see [`Self::run_control`]).
    tx_active: bool,
    /// A rollback was queued by `Transaction` drop and will be applied on
    /// the next interaction or at close.
    pending_rollback: bool,
    /// How long a statement blocked by another connection's open
    /// transaction waits before failing with SQLITE_BUSY
    /// ("database is locked"). sqlx-sqlite parity: 5 seconds.
    busy_timeout: Duration,
    /// Bounded cache: SQL text → gate wait mode (`Some(true)` = read
    /// gating, `Some(false)` = write gating, `None` = nothing executable).
    /// Avoids re-parsing every statement on every fetch.
    gate_cache: StdHashMap<String, Option<bool>>,
    log_settings: LogSettings,
}

impl std::fmt::Debug for RustqliteConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RustqliteConnection")
            .field("id", &self.id)
            .field("tx_depth", &self.tx_depth)
            .finish_non_exhaustive()
    }
}

/// How a parsed statement executes (mirrors the C-ABI compat layer's
/// proven classification).
enum Kind {
    /// SELECT / EXPLAIN — engine streaming statement under a read lock.
    Rows,
    /// PRAGMA without a value — buffered query form.
    PragmaRead,
    /// INSERT / UPDATE / DELETE (possibly with RETURNING) — engine
    /// statement under the write lock.
    Dml,
    /// Transaction control, DDL, ATTACH, VACUUM — one-shot via the mutable
    /// path.
    Once,
}

fn classify(stmt: &Ast) -> Kind {
    match stmt {
        Ast::Select(_) | Ast::Explain(_) => Kind::Rows,
        Ast::Pragma(p) => {
            if p.value.is_some() {
                Kind::Once
            } else {
                Kind::PragmaRead
            }
        }
        // DML: engine statement (may have RETURNING).
        Ast::Insert(_) | Ast::Update(_) | Ast::Delete(_) => Kind::Dml,
        // Transaction control, DDL, ATTACH, VACUUM, savepoint ops.
        _ => Kind::Once,
    }
}

/// Hot-path statement classification WITHOUT a full parse.
///
/// `run_one` used to `parser::parse` EVERY statement just to pick an
/// execution arm — ~1 µs of AST allocation (identifier Strings, expression
/// trees) per INSERT/SELECT, pure overhead on the dominant bind-and-execute
/// loop (measured in `examples/probe_driver_layer.rs`: parse 1.0 µs vs the
/// engine's own 0.8 µs INSERT). The `Rows` and `Dml` arms never touch the
/// AST at all, so statements whose first keyword unambiguously determines
/// the arm skip the parse entirely; the engine's own statement cache
/// (SQL text → compiled plan) still parses once on first sight.
///
/// Everything ambiguous — WITH-prefix CTEs (SELECT or INSERT?), EXPLAIN,
/// PRAGMA (value vs read), BEGIN/COMMIT, DDL, comments-first scripts —
/// falls back to the full parse, which cold statements can afford.
enum HeadKind {
    Rows,
    Dml,
    /// Not decidable from the head keyword: parse and `classify`.
    NeedsParse,
}

fn classify_head(sql: &str) -> HeadKind {
    let b = sql.as_bytes();
    let mut i = 0usize;
    // Skip whitespace and comments before the first token.
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < b.len() && b[i] == b'-' && b[i + 1] == b'-' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        break;
    }
    // Extract the leading keyword (ASCII letters only — SQL keywords).
    let start = i;
    while i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'_') {
        i += 1;
    }
    if i == start {
        return HeadKind::NeedsParse; // starts with '(' VALUES-row, digit, ...
    }
    let kw = &b[start..i];
    fn eq_ignore_case(a: &[u8], kw: &[u8]) -> bool {
        if a.len() != kw.len() {
            return false;
        }
        for k in 0..a.len() {
            if !a[k].eq_ignore_ascii_case(&kw[k]) {
                return false;
            }
        }
        true
    }
    if eq_ignore_case(b"SELECT", kw) {
        HeadKind::Rows
    } else if eq_ignore_case(b"INSERT", kw)
        || eq_ignore_case(b"REPLACE", kw)
        || eq_ignore_case(b"UPDATE", kw)
        || eq_ignore_case(b"DELETE", kw)
    {
        HeadKind::Dml
    } else {
        // WITH / EXPLAIN / PRAGMA / BEGIN / COMMIT / CREATE / ... — the
        // full parse decides (and the Once arm needs the AST anyway).
        HeadKind::NeedsParse
    }
}

/// Which transaction-gate wait mode does this SQL need before execution?
///
/// * `None` — no executable statements (blank/comment-only).
/// * `Some(true)` — read gating: wait only while a foreign transaction has
///   uncommitted writes (read-only foreign transactions never block).
/// * `Some(false)` — write gating: wait while ANY foreign transaction is
///   open. A multi-statement script is write-gated if ANY of its
///   statements is a write/DDL/control statement.
fn gate_mode_for(sql: &str) -> Option<bool> {
    let mut pos = 0usize;
    let mut any_read = false;
    let mut any_write = false;
    loop {
        let (stmt_text, off) = split_first_stmt(&sql[pos..]);
        pos += off;
        let Some(stmt_text) = stmt_text else {
            break;
        };
        match parser::parse(&stmt_text) {
            Ok(ast) => match classify(&ast) {
                Kind::Rows | Kind::PragmaRead => any_read = true,
                _ => any_write = true,
            },
            Err(_) => any_write = true, // conservative: treat as write
        }
        if pos >= sql.len() {
            break;
        }
    }
    if !any_read && !any_write {
        None
    } else {
        Some(!any_write)
    }
}

/// Engine read guard with the thread's read-view preference armed by
/// `acquire_read` (`Committed` for foreign-transaction committed-view
/// reads, `Live` otherwise) and restored to `Auto` on drop. All driver
/// engine calls run synchronously inside the guard's scope, so the
/// thread-local preference never leaks across an await point.
struct ReadGuardView<'a> {
    guard: parking_lot::RwLockReadGuard<'a, Engine>,
}

impl std::ops::Deref for ReadGuardView<'_> {
    type Target = Engine;
    #[inline]
    fn deref(&self) -> &Engine {
        &self.guard
    }
}

impl Drop for ReadGuardView<'_> {
    #[inline]
    fn drop(&mut self) {
        Engine::set_read_view_public(crate::api::ReadView::Auto);
    }
}

impl RustqliteConnection {
    /// Open a connection with the given options (see
    /// [`RustqliteConnectOptions`]).
    pub fn open(options: &RustqliteConnectOptions) -> Result<Self, SqlxError> {
        let shared = acquire_shared(options)?;
        let conn = RustqliteConnection {
            shared,
            id: next_conn_id(),
            tx_depth: 0,
            tx_active: false,
            pending_rollback: false,
            busy_timeout: options.busy_timeout,
            gate_cache: StdHashMap::new(),
            log_settings: options.log_settings.clone(),
        };
        // sqlx-sqlite parity: foreign keys ON by default. This runs at
        // connection SETUP: take the raw write lock WITHOUT the
        // transaction gate — a foreign transaction that is open right now
        // (possibly a leaked one awaiting pool-release cleanup) must never
        // block a new connection from opening.
        if options.foreign_keys {
            let mut db = conn.shared.db.write();
            db.execute("PRAGMA foreign_keys = ON", &[] as &[EngineValue])
                .map_err(crate::sqlx_driver::error::engine_err)?;
        }
        Ok(conn)
    }

    /// The current transaction depth (0 = autocommit).
    pub fn transaction_depth(&self) -> usize {
        self.tx_depth
    }

    /// Set the busy timeout: how long a statement blocked by another
    /// connection's open transaction waits before failing with
    /// SQLITE_BUSY ("database is locked").
    pub fn set_busy_timeout(&mut self, timeout: Duration) {
        self.busy_timeout = timeout;
    }

    /// The configured busy timeout.
    pub fn busy_timeout(&self) -> Duration {
        self.busy_timeout
    }

    // -- lock acquisition --------------------------------------------------

    /// Acquire the engine read lock, with WAL-grade committed-view
    /// passthrough: while a foreign transaction with uncommitted writes is
    /// open, readers that can be served the BEGIN-time (committed) state
    /// proceed WITHOUT waiting — the engine arms the committed view for
    /// this thread (exact connection identity, thread-id heuristic
    /// defeated). Foreign transactions with DDL/savepoints (or anything
    /// that invalidates committed-view reconstruction) still wait for the
    /// transaction to end, exactly like a rollback-journal SQLite. The
    /// thread's read-view preference is restored when the guard drops.
    fn acquire_read(&self) -> Result<ReadGuardView<'_>, SqlxError> {
        let deadline = std::time::Instant::now() + self.busy_timeout;
        loop {
            let db = self.shared.db.read();
            let blocked = self
                .shared
                .foreign_tx_blocked(self.id, /* only_dirty: */ true);
            let committed = blocked && db.committed_reads_available();
            if !blocked || committed {
                if committed {
                    Engine::set_read_view_public(crate::api::ReadView::Committed);
                } else {
                    // Exact identity: this connection is not the foreign
                    // reader (no txn, or our own txn) — force the LIVE view
                    // so the engine's thread heuristic can never serve a
                    // migrated owner-task the BEGIN-time state.
                    Engine::set_read_view_public(crate::api::ReadView::Live);
                }
                return Ok(ReadGuardView { guard: db });
            }
            drop(db);
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            if !self.shared.wait_tx_clear(self.id, true, remain) {
                return Err(crate::sqlx_driver::error::busy());
            }
        }
    }

    /// Acquire the engine write lock, waiting (up to the busy timeout)
    /// while another connection's transaction is open. Mirrors SQLite's
    /// busy handler: the write proceeds as soon as the other connection
    /// commits or rolls back; only on timeout does it fail with
    /// SQLITE_BUSY.
    fn acquire_write(&self) -> Result<parking_lot::RwLockWriteGuard<'_, Engine>, SqlxError> {
        let deadline = std::time::Instant::now() + self.busy_timeout;
        loop {
            // Pre-wait outside the lock: the gate condvar parks us instead
            // of spinning on the RwLock.
            if self.shared.foreign_tx(self.id) {
                let remain = deadline.saturating_duration_since(std::time::Instant::now());
                if !self.shared.wait_tx_clear(self.id, false, remain) {
                    return Err(crate::sqlx_driver::error::busy());
                }
            }
            let db = self.shared.db.write();
            if self.gate_write(&db).is_ok() {
                return Ok(db);
            }
            // A foreign transaction opened between the wait and the lock;
            // drop the guard and wait again (the next round times out if
            // the deadline has passed).
            drop(db);
        }
    }

    // -- execution ---------------------------------------------------------

    /// Determine whether the statements in `sql` need the transaction gate
    /// (and which kind), then wait for it COOPERATIVELY (waker-based: the
    /// async runtime keeps running while we wait, unlike a condvar park).
    ///
    /// Returns `Ok(())` when the gate is passable (or was never needed);
    /// `Err(SQLITE_BUSY)` when the busy timeout expired first.
    async fn wait_gate_for(&mut self, sql: &SqlStr) -> Result<(), SqlxError> {
        let only_dirty = match self.gate_cache.get(sql.as_str()) {
            Some(m) => *m,
            None => {
                let m = gate_mode_for(sql.as_str());
                // Bound the cache: SQL texts are unbounded in the general
                // case; 512 distinct statements is far beyond typical use.
                if self.gate_cache.len() >= 512 {
                    self.gate_cache.clear();
                }
                self.gate_cache.insert(sql.as_str().to_string(), m);
                m
            }
        };
        let Some(only_dirty) = only_dirty else {
            return Ok(());
        };
        if !self.shared.foreign_tx_blocked(self.id, only_dirty) {
            return Ok(()); // fast path: nothing to wait for
        }
        // WAL-grade committed reads: a foreign dirty transaction whose
        // BEGIN-time state the engine can reconstruct serves this reader
        // WITHOUT waiting. The peek takes the engine read lock — which
        // blocks at most one in-flight writer statement (engine calls
        // are synchronous under the lock, never awaiting), so the async
        // thread parks only for a bounded statement, not the txn.
        if only_dirty {
            let pass = {
                let db = self.shared.db.read();
                db.committed_reads_available()
            };
            if pass {
                return Ok(());
            }
        }
        gate::GateWait::new(
            Arc::clone(&self.shared),
            self.id,
            only_dirty,
            self.busy_timeout,
        )
        .await
    }

    /// Execute a full sqlx query synchronously and return the stream items
    /// (query results interleaved with rows, exactly like sqlx-sqlite's
    /// `fetch_many` stream).
    fn execute_sync(
        &mut self,
        sql: &SqlStr,
        arguments: Option<RustqliteArguments>,
        limit_one: bool,
    ) -> Result<Vec<Either<RustqliteQueryResult, RustqliteRow>>, SqlxError> {
        self.apply_pending_rollback()?;

        let is_script = arguments.is_none();
        let values = arguments.map(|a| a.values).unwrap_or_default();

        if is_script {
            // Raw script mode (raw_sql / execute with no binds): may
            // contain multiple statements.
            let script = sql.as_str();
            let mut pos = 0usize;
            let mut out = Vec::new();
            loop {
                let (stmt_text, off) = split_first_stmt(&script[pos..]);
                pos += off;
                let Some(stmt_text) = stmt_text else {
                    break;
                };
                let items = self.run_one(&stmt_text, sql, &values, limit_one)?;
                out.extend(items);
                if limit_one && out.iter().any(|e| matches!(e, Either::Right(_))) {
                    return Ok(out);
                }
                if pos >= script.len() {
                    break;
                }
            }
            Ok(out)
        } else {
            // Prepared mode: exactly one statement allowed.
            let (stmt_text, off) = split_first_stmt(sql.as_str());
            let Some(stmt_text) = stmt_text else {
                return Ok(vec![Either::Left(RustqliteQueryResult::default())]);
            };
            let tail = &sql.as_str()[off..];
            if !is_blank_chunk(tail) {
                return Err(SqlxError::Protocol(format!(
                    "sqlx sent a multi-statement query with bound arguments ({} bytes of trailing statements); \
                     use `raw_sql()` for multi-statement scripts",
                    tail.trim().len()
                )));
            }
            self.run_one(&stmt_text, sql, &values, limit_one)
        }
    }

    /// Run one statement. `sql` is the ORIGINAL full script (for logging).
    fn run_one(
        &mut self,
        stmt_sql: &str,
        script: &SqlStr,
        values: &[EngineValue],
        limit_one: bool,
    ) -> Result<Vec<Either<RustqliteQueryResult, RustqliteRow>>, SqlxError> {
        // Hot-path classification: the head keyword decides the arm for
        // SELECT / INSERT / UPDATE / DELETE / REPLACE without the ~1 µs
        // AST parse (see classify_head). Only the Once arm needs the AST,
        // so the parse happens there (or below when the head is ambiguous).
        let mut parsed: Option<Ast> = None;
        let kind = match classify_head(stmt_sql) {
            HeadKind::Rows => Kind::Rows,
            HeadKind::Dml => Kind::Dml,
            HeadKind::NeedsParse => {
                let ast = parser::parse(stmt_sql).map_err(crate::sqlx_driver::error::engine_err)?;
                let kind = classify(&ast);
                if matches!(kind, Kind::Once) {
                    parsed = Some(ast);
                }
                kind
            }
        };

        let mut logger = QueryLogger::new(script.clone(), self.log_settings.clone());

        let mut out: Vec<Either<RustqliteQueryResult, RustqliteRow>> = Vec::new();
        let mut rows_affected: u64 = 0;
        let mut last_rowid: i64 = 0;

        match kind {
            Kind::Rows => {
                let db = self.acquire_read()?;
                if limit_one {
                    // Prepare + one step: early-exit for fetch_optional.
                    let mut stmt = db
                        .prepare(stmt_sql)
                        .map_err(crate::sqlx_driver::error::engine_err)?;
                    stmt.bind_all(values)
                        .map_err(crate::sqlx_driver::error::engine_err)?;
                    if stmt.step().map_err(crate::sqlx_driver::error::engine_err)?
                        == StepResult::Row
                    {
                        let (columns, names) = columns_from_names(&stmt_column_names(&stmt));
                        if let Some(row) = stmt.row() {
                            out.push(Either::Right(RustqliteRow::new(
                                row.clone(),
                                &columns,
                                &names,
                            )));
                            logger.increment_rows_returned();
                        }
                    }
                } else {
                    let (names, rows) = db
                        .query_with_columns(stmt_sql, values)
                        .map_err(crate::sqlx_driver::error::engine_err)?;
                    let (columns, col_names) = columns_from_names(&names);
                    for row in rows {
                        out.push(Either::Right(RustqliteRow::new(row, &columns, &col_names)));
                        logger.increment_rows_returned();
                        if limit_one {
                            break;
                        }
                    }
                }
            }
            Kind::PragmaRead => {
                let db = self.acquire_read()?;
                let (names, rows) = db
                    .query_with_columns(stmt_sql, values)
                    .map_err(crate::sqlx_driver::error::engine_err)?;
                let (columns, col_names) = columns_from_names(&names);
                for row in rows {
                    out.push(Either::Right(RustqliteRow::new(row, &columns, &col_names)));
                    logger.increment_rows_returned();
                    if limit_one {
                        break;
                    }
                }
            }
            Kind::Dml => {
                let mut db = self.acquire_write()?;
                // A DEFERRED (sqlx) transaction starts its engine-level
                // transaction here, at the FIRST write statement — readers
                // inside never-started deferred transactions stay fully
                // concurrent until this moment.
                if self.tx_active && !db.in_transaction.load(Ordering::Acquire) {
                    db.execute("BEGIN", &[] as &[EngineValue])
                        .map_err(crate::sqlx_driver::error::engine_err)?;
                    self.shared.tx_owner.store(self.id, Ordering::Release);
                }
                // Mark the open transaction dirty BEFORE the first mutation:
                // readers that arrive mid-transaction must wait instead of
                // observing uncommitted rows. (Harmless in autocommit: with
                // no transaction owner, readers ignore the dirty flag.)
                self.shared.tx_dirty.store(true, Ordering::Release);
                let mut stmt = db
                    .prepare(stmt_sql)
                    .map_err(crate::sqlx_driver::error::engine_err)?;
                stmt.bind_all(values)
                    .map_err(crate::sqlx_driver::error::engine_err)?;
                let (mut columns, mut names): (Option<Columns>, Option<NameMap>) = (None, None);
                loop {
                    match stmt.step() {
                        Ok(StepResult::Row) => {
                            if columns.is_none() {
                                let (c, n) = columns_from_names(&stmt_column_names(&stmt));
                                columns = Some(c);
                                names = Some(n);
                            }
                            if let Some(row) = stmt.row() {
                                out.push(Either::Right(RustqliteRow::new(
                                    row.clone(),
                                    columns.as_ref().unwrap(),
                                    names.as_ref().unwrap(),
                                )));
                                logger.increment_rows_returned();
                            }
                            if limit_one {
                                break;
                            }
                        }
                        Ok(StepResult::Done) => break,
                        Err(e) => return Err(crate::sqlx_driver::error::engine_err(e)),
                    }
                }
                rows_affected = stmt.changes().max(0) as u64;
                last_rowid = db.last_insert_rowid();
            }
            Kind::Once => {
                // The AST for the Once arm: either parsed above (ambiguous
                // head keyword classified as Once) or parsed now — cheap,
                // control/DDL statements are never hot-loop statements.
                let ast = match parsed {
                    Some(ast) => ast,
                    None => {
                        parser::parse(stmt_sql).map_err(crate::sqlx_driver::error::engine_err)?
                    }
                };
                let mut db = self.acquire_write()?;
                // A DEFERRED (sqlx) transaction that issues DDL or another
                // once-class statement starts its engine-level transaction
                // here (keeps DDL inside the deferred tx transactional, like
                // SQLite). A raw `BEGIN` statement starts it directly.
                let is_begin = matches!(&ast, Ast::Begin(_));
                if !is_begin && self.tx_active && !db.in_transaction.load(Ordering::Acquire) {
                    db.execute("BEGIN", &[] as &[EngineValue])
                        .map_err(crate::sqlx_driver::error::engine_err)?;
                    self.shared.tx_owner.store(self.id, Ordering::Release);
                }
                // Writes/DDL inside a transaction make it dirty (readers must
                // wait); harmless in autocommit.
                self.shared.tx_dirty.store(true, Ordering::Release);
                db.execute(stmt_sql, values)
                    .map_err(crate::sqlx_driver::error::engine_err)?;
                // Transaction bookkeeping for raw SQL control statements.
                match &ast {
                    Ast::Begin(_) => {
                        // A fresh transaction starts clean.
                        self.shared.tx_owner.store(self.id, Ordering::Release);
                        self.shared.tx_dirty.store(false, Ordering::Release);
                        self.shared.notify_tx_change();
                    }
                    Ast::Commit => {
                        if self.i_own_engine_tx() {
                            self.shared.tx_reset();
                        }
                    }
                    // Only a FULL rollback ends the transaction;
                    // `ROLLBACK TO SAVEPOINT` keeps it open.
                    Ast::Rollback(r) if r.savepoint.is_none() && self.i_own_engine_tx() => {
                        self.shared.tx_reset();
                    }
                    Ast::Rollback(_) => {}
                    _ => {}
                }
                rows_affected = db.changes().max(0) as u64;
                last_rowid = db.last_insert_rowid();
            }
        }

        logger.increase_rows_affected(rows_affected);
        logger.finish();

        // Every statement ends with its query result (SQLite semantics:
        // SELECTs report 0 rows affected).
        out.push(Either::Left(RustqliteQueryResult {
            rows_affected,
            last_insert_rowid: last_rowid,
        }));

        Ok(out)
    }

    /// Run a transaction control statement (BEGIN / COMMIT / SAVEPOINT / ...).
    ///
    /// **DEFERRED transactions (SQLite `BEGIN DEFERRED` semantics, which is
    /// what sqlx's `Transaction` API uses):** `BEGIN` only records the
    /// intent — it does NOT start the engine-level transaction and does
    /// not claim the transaction gate. Reads inside a deferred
    /// transaction run as ordinary (gated) reads, so MANY connections can
    /// hold read transactions simultaneously. The engine-level
    /// transaction is started lazily by the FIRST write statement, which
    /// claims the single engine transaction (other connections then wait
    /// per the busy timeout). `COMMIT`/`ROLLBACK` of a never-started
    /// transaction is a no-op. This is what makes the classic
    /// read-tx-everywhere pool pattern fully concurrent.
    fn run_control(&mut self, sql: &str) -> Result<(), SqlxError> {
        // A queued rollback must land before any new BEGIN.
        self.apply_pending_rollback()?;
        let ast = parser::parse(sql).map_err(crate::sqlx_driver::error::engine_err)?;
        match &ast {
            Ast::Begin(_) => {
                // DEFERRED: record intent only. The engine tx starts at the
                // first write statement (see the Dml arm in `run_one`).
                self.tx_active = true;
            }
            Ast::Commit => {
                self.tx_active = false;
                if self.i_own_engine_tx() {
                    let mut db = self.acquire_write()?;
                    db.execute("COMMIT", &[] as &[EngineValue])
                        .map_err(crate::sqlx_driver::error::engine_err)?;
                    drop(db);
                    self.shared.tx_reset();
                }
                // else: read-only deferred tx — nothing to commit.
            }
            Ast::Rollback(r) => {
                if r.savepoint.is_some() {
                    // `ROLLBACK TO SAVEPOINT` — the transaction stays open.
                    if self.i_own_engine_tx() {
                        let mut db = self.acquire_write()?;
                        let _ = db.execute(sql, &[] as &[EngineValue]);
                    }
                } else {
                    self.tx_active = false;
                    if self.i_own_engine_tx() {
                        let mut db = self.acquire_write()?;
                        let _ = db.execute("ROLLBACK", &[] as &[EngineValue]);
                        drop(db);
                        self.shared.tx_reset();
                    }
                    // else: never-started deferred tx — nothing to roll back.
                }
            }
            Ast::Savepoint(_) | Ast::Release(_) => {
                // Savepoints only exist inside a STARTED engine transaction.
                // A deferred transaction that has not written yet has no
                // engine transaction, so its savepoints are no-ops (they
                // guard nothing: there are no changes to unwind).
                if self.i_own_engine_tx() {
                    let mut db = self.acquire_write()?;
                    db.execute(sql, &[] as &[EngineValue])
                        .map_err(crate::sqlx_driver::error::engine_err)?;
                    drop(db);
                }
            }
            // ROLLBACK TO SAVEPOINT arrives as a plain Rollback statement in
            // this parser; other statements (PRAGMA etc.) execute directly.
            _ => {
                let mut db = self.acquire_write()?;
                db.execute(sql, &[] as &[EngineValue])
                    .map_err(crate::sqlx_driver::error::engine_err)?;
            }
        }
        Ok(())
    }

    /// True when THIS connection currently owns the engine-level transaction.
    fn i_own_engine_tx(&self) -> bool {
        self.shared.tx_owner.load(Ordering::Acquire) == self.id
    }

    /// SQLite BUSY semantics, enforced under the engine write lock: while
    /// another connection owns the open engine transaction, this
    /// connection's writes are refused. [`Self::acquire_write`] turns the
    /// refusal into a bounded wait (busy timeout) first.
    fn gate_write(&self, db: &Engine) -> Result<(), SqlxError> {
        let in_tx = db.in_transaction.load(Ordering::Acquire);
        let owner = self.shared.tx_owner.load(Ordering::Acquire);
        if in_tx && owner != 0 && owner != self.id {
            return Err(crate::sqlx_driver::error::driver_err("database is locked"));
        }
        Ok(())
    }

    /// Apply a rollback queued by a dropped `Transaction`.
    fn apply_pending_rollback(&mut self) -> Result<(), SqlxError> {
        if !self.pending_rollback {
            return Ok(());
        }
        self.pending_rollback = false;
        if self.tx_depth > 0 {
            self.tx_active = false;
            if self.i_own_engine_tx() {
                let mut db = self.acquire_write()?;
                let _ = db.execute("ROLLBACK", &[] as &[EngineValue]);
                drop(db);
                self.shared.tx_reset();
            }
            self.tx_depth = 0;
        }
        Ok(())
    }

    /// Roll back any open transaction and release engine ownership.
    /// Called from `close()` and `Drop`.
    fn finalize(&mut self) {
        let _ = self.apply_pending_rollback();
        self.tx_active = false;
        // Roll back ANY engine-level transaction this connection left
        // open — including raw-script BEGINs that sqlx's tx_depth knows
        // nothing about — so a dropped connection can never leak a
        // transaction that would wedge every other connection.
        let i_own_engine_tx = {
            let db = self.shared.db.read();
            db.in_transaction.load(Ordering::Acquire)
                && self.shared.tx_owner.load(Ordering::Acquire) == self.id
        };
        if self.tx_depth > 0 || i_own_engine_tx {
            // best-effort: bypass the busy wait at close time
            if let Ok(mut db) = self.try_write_now() {
                let _ = db.execute("ROLLBACK", &[] as &[EngineValue]);
                drop(db);
            }
            self.tx_depth = 0;
        }
        if self.shared.tx_owner.load(Ordering::Acquire) == self.id {
            self.shared.tx_reset();
        } else {
            // We may still hold the engine in a transaction without owning
            // the driver-level bookkeeping (e.g. a raw BEGIN script that
            // failed mid-way): make sure waiters are re-checked.
            self.shared.notify_tx_change();
        }
    }

    /// Acquire the write lock with ZERO busy wait (for close/drop paths
    /// where failing fast is better than parking a dropping task).
    fn try_write_now(&self) -> Result<parking_lot::RwLockWriteGuard<'_, Engine>, SqlxError> {
        let db = self.shared.db.write();
        if self.gate_write(&db).is_ok() {
            return Ok(db);
        }
        drop(db);
        // Someone else owns the tx; their commit/rollback will restore
        // consistency — our own tx cannot be the foreign one here, since
        // the foreign tx belongs to a connection that is still alive.
        Err(crate::sqlx_driver::error::busy())
    }
}

impl Drop for RustqliteConnection {
    fn drop(&mut self) {
        self.finalize();
    }
}

fn stmt_column_names(stmt: &EngineStatement<'_>) -> Vec<String> {
    let n = stmt.column_count();
    (0..n)
        .filter_map(|i| stmt.column_name(i).map(str::to_string))
        .collect()
}

impl SqlxConnection for RustqliteConnection {
    type Database = Rustqlite;
    type Options = RustqliteConnectOptions;

    fn close(self) -> impl Future<Output = Result<(), SqlxError>> + Send + 'static {
        let mut me = self;
        async move {
            me.finalize();
            Ok(())
        }
    }

    fn close_hard(self) -> impl Future<Output = Result<(), SqlxError>> + Send + 'static {
        let mut me = self;
        async move {
            me.finalize();
            Ok(())
        }
    }

    fn ping(&mut self) -> impl Future<Output = Result<(), SqlxError>> + Send + '_ {
        // The engine is in-process and always reachable. sqlx-core pings a
        // connection when it is RETURNED TO THE POOL ("flush anything
        // time-sensitive like transaction rollbacks"): use that hook to
        // guarantee an idle pooled connection never leaks a transaction
        // onto the shared engine.
        //
        // * tx_depth == 0 (no sqlx-managed Transaction alive) + a deferred
        //   intent or an engine-level tx we own (raw `BEGIN` script that
        //   escaped) → roll it back now.
        // * tx_depth > 0 → a live `Transaction` object exists; leave it
        //   alone (its own Drop handles rollback).
        if self.tx_depth == 0 {
            self.tx_active = false;
            if self.i_own_engine_tx() {
                if let Ok(mut db) = self.try_write_now() {
                    let _ = db.execute("ROLLBACK", &[] as &[EngineValue]);
                    drop(db);
                    self.shared.tx_reset();
                }
            }
        }
        future::ready(Ok(()))
    }

    fn begin(
        &mut self,
    ) -> impl Future<
        Output = Result<sqlx_core::transaction::Transaction<'_, Self::Database>, SqlxError>,
    > + Send
           + '_ {
        sqlx_core::transaction::Transaction::begin(self, None)
    }

    fn shrink_buffers(&mut self) {
        // No incremental buffers: rows are returned owned.
    }

    fn flush(&mut self) -> impl Future<Output = Result<(), SqlxError>> + Send + '_ {
        // Nothing is queued: execution is inline.
        future::ready(Ok(()))
    }

    fn should_flush(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

impl<'c> SqlxExecutor<'c> for &'c mut RustqliteConnection {
    type Database = Rustqlite;

    fn fetch_many<'e, 'q: 'e, E>(
        self,
        mut query: E,
    ) -> BoxStream<'e, Result<Either<RustqliteQueryResult, RustqliteRow>, SqlxError>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Rustqlite>,
    {
        let arguments = match query.take_arguments() {
            Ok(a) => a,
            Err(e) => {
                return stream::once(future::ready(Err(SqlxError::Encode(e)))).boxed();
            }
        };
        let sql = query.sql();

        Box::pin(
            stream::once(async move {
                // Cooperative gate wait: if another connection's transaction
                // currently blocks this statement, await (waker-based — never
                // blocks the runtime thread) instead of parking the executor.
                self.wait_gate_for(&sql).await?;
                self.execute_sync(&sql, arguments, false)
            })
            .flat_map(|res| match res {
                Ok(items) => stream::iter(items.into_iter().map(Ok)).boxed(),
                Err(e) => stream::once(future::ready(Err(e))).boxed(),
            }),
        )
    }

    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        mut query: E,
    ) -> BoxFuture<'e, Result<Option<RustqliteRow>, SqlxError>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Rustqlite>,
    {
        let arguments = match query.take_arguments() {
            Ok(a) => a,
            Err(e) => return future::ready(Err(SqlxError::Encode(e))).boxed(),
        };
        let sql = query.sql();

        Box::pin(async move {
            self.wait_gate_for(&sql).await?;
            let items = self.execute_sync(&sql, arguments, true)?;
            Ok(items.into_iter().find_map(|either| match either {
                Either::Right(row) => Some(row),
                Either::Left(_) => None,
            }))
        })
    }

    fn prepare_with<'e>(
        self,
        sql: SqlStr,
        _parameters: &[RustqliteTypeInfo],
    ) -> BoxFuture<'e, Result<RustqliteStatement, SqlxError>>
    where
        'c: 'e,
    {
        Box::pin(async move {
            let stmt_sql = sql.as_str();
            let ast = parser::parse(stmt_sql).map_err(crate::sqlx_driver::error::engine_err)?;
            let kind = classify(&ast);

            let (param_count, names): (usize, Vec<String>) = match kind {
                Kind::Rows | Kind::Dml => {
                    let db = self.acquire_read()?;
                    let stmt = db
                        .prepare(stmt_sql)
                        .map_err(crate::sqlx_driver::error::engine_err)?;
                    let names = stmt_column_names(&stmt);
                    (stmt.parameter_count(), names)
                }
                _ => (0, Vec::new()),
            };
            let (columns, column_names) = columns_from_names(&names);
            Ok(RustqliteStatement {
                sql,
                param_count,
                columns,
                column_names,
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Transaction manager
// ---------------------------------------------------------------------------

/// Implementation of [`SqlxTransactionManager`] for rustqlite.
pub struct RustqliteTransactionManager;

impl SqlxTransactionManager for RustqliteTransactionManager {
    type Database = Rustqlite;

    async fn begin(
        conn: &mut RustqliteConnection,
        statement: Option<SqlStr>,
    ) -> Result<(), SqlxError> {
        let sql = statement.unwrap_or_else(|| begin_ansi_transaction_sql(conn.tx_depth));
        conn.run_control(sql.as_str())?;
        conn.tx_depth += 1;
        Ok(())
    }

    async fn commit(conn: &mut RustqliteConnection) -> Result<(), SqlxError> {
        if conn.tx_depth == 0 {
            return Err(SqlxError::Protocol(
                "commit called with no active transaction".into(),
            ));
        }
        let sql = commit_ansi_transaction_sql(conn.tx_depth);
        conn.run_control(sql.as_str())?;
        conn.tx_depth -= 1;
        Ok(())
    }

    async fn rollback(conn: &mut RustqliteConnection) -> Result<(), SqlxError> {
        if conn.tx_depth == 0 {
            return Err(SqlxError::Protocol(
                "rollback called with no active transaction".into(),
            ));
        }
        let sql = rollback_ansi_transaction_sql(conn.tx_depth);
        conn.run_control(sql.as_str())?;
        conn.tx_depth -= 1;
        Ok(())
    }

    fn start_rollback(conn: &mut RustqliteConnection) {
        // The engine is in-process, so we can roll back EAGERLY instead of
        // queueing: a dropped `Transaction` releases the engine-level
        // transaction immediately, which keeps other pool connections
        // from hitting `database is locked` until this one is reused.
        let _ = conn.apply_pending_rollback();
        if conn.tx_depth > 1 {
            // Restore to the enclosing savepoint (the savepoint stays
            // registered for the outer transaction's commit/rollback).
            let sql = rollback_ansi_transaction_sql(conn.tx_depth);
            let _ = conn.run_control(sql.as_str());
            conn.tx_depth -= 1;
        } else if conn.tx_depth == 1 {
            let _ = conn.run_control("ROLLBACK");
            conn.tx_depth = 0;
        }
    }

    fn get_transaction_depth(conn: &RustqliteConnection) -> usize {
        conn.tx_depth
    }
}

// ---------------------------------------------------------------------------
// Connect options
// ---------------------------------------------------------------------------

/// Connection options for rustqlite.
///
/// Built from a URL with the `rustqlite` scheme, or programmatically.
///
/// | URL | Meaning |
/// |-----|---------|
/// | `rustqlite::memory:` | private in-memory DB (per connection) |
/// | `rustqlite://:memory:?cache=shared` | shared in-memory DB (pool-friendly) |
/// | `rustqlite://app.db` | file DB |
/// | `rustqlite://app.db?mode=rwc` | file DB, create if missing |
/// | `...?foreign_keys=false` | disable FK enforcement for the connection |
#[derive(Clone, Debug)]
pub struct RustqliteConnectOptions {
    pub(crate) filename: PathBuf,
    pub(crate) in_memory: bool,
    pub(crate) shared_cache: bool,
    pub(crate) create_if_missing: bool,
    pub(crate) foreign_keys: bool,
    /// How long statements blocked by another connection's open
    /// transaction wait before failing with SQLITE_BUSY (default 5s,
    /// matching sqlx-sqlite's busy timeout).
    pub(crate) busy_timeout: Duration,
    pub(crate) log_settings: LogSettings,
}

impl Default for RustqliteConnectOptions {
    fn default() -> Self {
        Self {
            filename: PathBuf::from(":memory:"),
            in_memory: true,
            shared_cache: false,
            create_if_missing: false,
            foreign_keys: true,                   // sqlx-sqlite parity
            busy_timeout: Duration::from_secs(5), // sqlx-sqlite parity
            log_settings: LogSettings::default(),
        }
    }
}

impl RustqliteConnectOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the busy timeout — how long a statement blocked by another
    /// connection's open transaction waits before failing with
    /// SQLITE_BUSY ("database is locked").
    ///
    /// Examples: `Duration::ZERO` fails instantly (old behavior);
    /// 5 seconds (default) matches sqlx-sqlite.
    pub fn busy_timeout(mut self, timeout: Duration) -> Self {
        self.busy_timeout = timeout;
        self
    }

    /// Open a file database.
    pub fn filename(filename: impl Into<PathBuf>) -> Self {
        Self {
            filename: filename.into(),
            in_memory: false,
            shared_cache: false,
            ..Self::default()
        }
    }

    /// Use an in-memory database.
    pub fn in_memory(mut self, in_memory: bool) -> Self {
        self.in_memory = in_memory;
        self
    }

    /// True if this connects to an in-memory database.
    pub fn is_in_memory(&self) -> bool {
        self.in_memory
    }

    /// True if the database is shared across connections (`cache=shared`).
    pub fn is_shared_cache(&self) -> bool {
        self.shared_cache
    }

    /// Share the in-memory database across all connections in this process
    /// (SQLite's `cache=shared` for `:memory:`). Required for multi-
    /// connection pools on `:memory:`.
    pub fn shared_cache(mut self, shared: bool) -> Self {
        self.shared_cache = shared;
        self
    }

    /// A NAMED shared in-memory database (SQLite's
    /// `file:NAME?mode=memory&cache=shared`): all connections that use the
    /// same name share one engine; different names are independent
    /// databases. Use this for multi-connection in-memory pools that must
    /// be isolated from other pools in the same process.
    pub fn shared_memory(name: impl Into<String>) -> Self {
        Self {
            filename: PathBuf::from(name.into()),
            in_memory: true,
            shared_cache: true,
            ..Self::default()
        }
    }

    /// Create the database file if it does not exist.
    pub fn create_if_missing(mut self, create: bool) -> Self {
        self.create_if_missing = create;
        self
    }

    /// Enable or disable foreign key enforcement (default: on, matching
    /// sqlx's SQLite driver).
    pub fn foreign_keys(mut self, on: bool) -> Self {
        self.foreign_keys = on;
        self
    }
}

impl FromStr for RustqliteConnectOptions {
    type Err = SqlxError;

    fn from_str(url: &str) -> Result<Self, SqlxError> {
        let rest = url
            .strip_prefix("rustqlite://")
            .or_else(|| url.strip_prefix("rustqlite:"))
            .ok_or_else(|| {
                SqlxError::Configuration(
                    format!("invalid connection URL for rustqlite, expected the `rustqlite` scheme: {url}").into(),
                )
            })?;

        let (path_part, query) = match rest.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (rest, None),
        };

        let filename = percent_decode(path_part);
        let mut options = if filename == ":memory:" {
            Self::default()
        } else if let Some(stripped) = filename.strip_prefix("file:") {
            // SQLite URI form: `file:NAME` (an in-memory database when
            // combined with `mode=memory&cache=shared`, a regular file
            // otherwise).
            if query.map(|q| q.contains("mode=memory")).unwrap_or(false) {
                Self::shared_memory(stripped)
            } else {
                Self::filename(stripped)
            }
        } else {
            Self::filename(filename)
        };

        if let Some(query) = query {
            for (key, value) in sqlx_core::url::form_urlencoded::parse(query.as_bytes()) {
                let key = key.as_ref();
                let value = value.as_ref();
                match key {
                    "mode" => match value {
                        "memory" => options.in_memory = true,
                        "rwc" => options.create_if_missing = true,
                        "rw" => {}
                        // Advisory, same as the C-ABI compat layer.
                        "ro" => {}
                        other => {
                            return Err(SqlxError::Configuration(
                                format!("unknown value for `mode`: {other}").into(),
                            ));
                        }
                    },
                    "cache" if value == "shared" => options.shared_cache = true,
                    "foreign_keys" => {
                        options.foreign_keys = matches!(value, "true" | "on" | "1" | "yes");
                    }
                    "busy_timeout" | "busy_timeout_ms" => {
                        // Milliseconds, like SQLite's PRAGMA busy_timeout.
                        options.busy_timeout = value
                            .parse::<u64>()
                            .ok()
                            .map(Duration::from_millis)
                            .unwrap_or(Duration::from_secs(5));
                    }
                    _ => {}
                }
            }
        }

        Ok(options)
    }
}

impl SqlxConnectOptions for RustqliteConnectOptions {
    type Connection = RustqliteConnection;

    fn from_url(url: &sqlx_core::url::Url) -> Result<Self, SqlxError> {
        Self::from_str(url.as_str())
    }

    fn to_url_lossy(&self) -> sqlx_core::url::Url {
        let mut url = String::from("rustqlite://");
        url.push_str(&percent_encode(&self.filename.to_string_lossy()));
        if self.in_memory && self.shared_cache {
            url.push_str("?cache=shared");
        } else if self.create_if_missing {
            url.push_str("?mode=rwc");
        }
        sqlx_core::url::Url::parse(&url).unwrap_or_else(|_| {
            sqlx_core::url::Url::parse("rustqlite::memory:").expect("valid URL")
        })
    }

    fn connect(&self) -> impl Future<Output = Result<RustqliteConnection, SqlxError>> + Send + '_ {
        let options = self.clone();
        async move { RustqliteConnection::open(&options) }
    }

    fn log_statements(mut self, level: LevelFilter) -> Self {
        self.log_settings.log_statements(level);
        self
    }

    fn log_slow_statements(mut self, level: LevelFilter, duration: Duration) -> Self {
        self.log_settings.log_slow_statements(level, duration);
        self
    }
}

/// Stable registry key for a database path.
///
/// The registry maps every spelling of a path (raw, symlinked, Windows
/// 8.3 short name, pre-creation) to ONE engine. The old key —
/// `canonicalize(path)` with a raw-path fallback — was unstable across
/// CONNECTION CREATION ORDER: connection #1 opens before the file exists,
/// so canonicalize fails and the key is the raw spelling; connection #2
/// opens after the engine created the file, canonicalize succeeds and
/// resolves symlinks (/var → /private/var on macOS) and short names
/// (RUNNER~1 on Windows) — two keys, TWO ENGINES on one file. The
/// count-cache/epoch incoherence that produced `COUNT(*) = 0` while
/// `SELECT *` saw every row (and the lost-write COUNT=9/10) came from
/// exactly this, and it split pool connections across engines that then
/// fought over the file.
fn canonical_key(path: &std::path::Path) -> String {
    // File exists: canonicalize resolves symlinks and short names.
    if let Ok(c) = std::fs::canonicalize(path) {
        return normalize_key(c);
    }
    // File not yet created: canonicalize the PARENT (which exists) and
    // join the file name — byte-identical to what canonicalize will
    // return once the file exists, so connection #1 and every later
    // connection agree on one engine.
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(cp) = std::fs::canonicalize(parent) {
            return normalize_key(cp.join(name));
        }
    }
    normalize_key(path.to_path_buf())
}

/// Fold a canonical path into a registry key string. On Windows,
/// canonicalize returns `\\?\`-prefixed verbatim paths while raw fallback
/// spellings are plain — normalize both to the plain form and fold case
/// (NTFS is case-insensitive) so every spelling maps to one engine.
#[cfg(windows)]
fn normalize_key(p: std::path::PathBuf) -> String {
    let mut s = p.to_string_lossy().replace('/', "\\");
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        s = format!(r"\\{rest}");
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    s.to_lowercase()
}

#[cfg(not(windows))]
fn normalize_key(p: std::path::PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

fn acquire_shared(options: &RustqliteConnectOptions) -> Result<Arc<SharedDb>, SqlxError> {
    let engine_err = |e: crate::error::Error| crate::sqlx_driver::error::engine_err(e);

    if options.in_memory && !options.shared_cache {
        // Private in-memory database (SQLite `:memory:` semantics).
        let db = Engine::open_in_memory().map_err(engine_err)?;
        return Ok(new_shared(db));
    }

    let key = if options.in_memory {
        if options.filename.as_os_str() != ":memory:" {
            // Named shared in-memory database
            // (SQLite's `file:NAME?mode=memory&cache=shared`).
            format!("mem:{}", options.filename.to_string_lossy())
        } else {
            // One process-wide shared `:memory:` database
            // (SQLite's shared-cache `:memory:`).
            "memdb:shared".to_string()
        }
    } else {
        // The parent directory must exist BEFORE the key is computed:
        // canonical_key's parent-join fallback needs a real directory to
        // resolve, and create_dir_all here is idempotent with the open
        // block below.
        if !options.filename.as_os_str().is_empty() && options.create_if_missing {
            if let Some(parent) = options.filename.parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }
        }
        canonical_key(&options.filename)
    };

    let mut map = engines().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = map.get(&key) {
        return Ok(Arc::clone(existing));
    }

    let db = if options.in_memory {
        Engine::open_in_memory().map_err(engine_err)?
    } else {
        if !options.filename.as_os_str().is_empty() && !options.filename.exists() {
            if !options.create_if_missing {
                return Err(crate::sqlx_driver::error::driver_err(
                    "unable to open database file",
                ));
            }
            if let Some(parent) = options.filename.parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }
        }
        Engine::open(&options.filename).map_err(engine_err)?
    };

    let shared = new_shared(db);
    map.insert(key, Arc::clone(&shared));
    Ok(shared)
}

// ---------------------------------------------------------------------------
// Statement splitting (multi-statement scripts) — same scanner as the
// C-ABI compat layer.
// ---------------------------------------------------------------------------

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

fn is_blank_chunk(s: &str) -> bool {
    // True when the chunk contains no executable SQL: only whitespace,
    // semicolons, and comments (`-- ...` / `/* ... */`). A trailing
    // comment-only chunk is NOT a statement (previously `-- foo` was
    // parsed as a statement and produced a parse error at Eof).
    let b: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if c == '-' && i + 1 < b.len() && b[i + 1] == '-' {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && i + 1 < b.len() && b[i + 1] == '*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else if !(c.is_whitespace() || c == ';') {
            return false;
        } else {
            i += 1;
        }
    }
    true
}

/// Find the first non-blank statement: returns (statement_text, tail_offset).
fn split_first_stmt(sql: &str) -> (Option<String>, usize) {
    let bytes = sql.as_bytes();
    let mut pos = 0usize;
    loop {
        let end = scan_stmt_end(&bytes[pos..]) + pos;
        let chunk = &sql[pos..end];
        if is_blank_chunk(chunk.trim_end_matches(';')) {
            if end >= sql.len() {
                return (None, sql.len());
            }
            pos = end;
            continue;
        }
        let stmt_text = chunk.trim_end();
        let stmt_text = stmt_text.strip_suffix(';').unwrap_or(stmt_text).trim_end();
        return (Some(stmt_text.to_string()), end);
    }
}

fn percent_decode(s: &str) -> String {
    sqlx_core::percent_encoding::percent_decode_str(s)
        .decode_utf8()
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

fn percent_encode(s: &str) -> String {
    use sqlx_core::percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

// ---------------------------------------------------------------------------
// Required macro impls (from sqlx-core, matching the official drivers)
// ---------------------------------------------------------------------------

sqlx_core::impl_into_arguments_for_arguments!(RustqliteArguments);
sqlx_core::impl_column_index_for_row!(RustqliteRow);
sqlx_core::impl_column_index_for_statement!(RustqliteStatement);
sqlx_core::impl_acquire!(Rustqlite, RustqliteConnection);

// required because some databases have a different handling of NULL
sqlx_core::impl_encode_for_option!(Rustqlite);

// ---------------------------------------------------------------------------
// Compile-time Send/Sync assertions
// ---------------------------------------------------------------------------

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RustqliteConnection>();
    assert_send_sync::<RustqliteRow>();
    assert_send_sync::<RustqliteStatement>();
    assert_send_sync::<RustqliteArguments>();
    assert_send_sync::<SharedDb>();
};

const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<futures_util::future::Ready<Result<(), SqlxError>>>();
};
