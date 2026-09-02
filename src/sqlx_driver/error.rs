//! Error type for the native sqlx driver.
//!
//! Constraint messages are byte-identical to SQLite's `sqlite3_errmsg`
//! output (the engine guarantees this), and the result codes carry
//! SQLite's extended codes — sqlx's `error_kind()` classification and
//! ORMs (sea-orm) pattern-match on exactly these.

use std::borrow::Cow;
use std::fmt;

use sqlx_core::error::{BoxDynError, DatabaseError, Error as SqlxError, ErrorKind};

/// The error type returned by the rustqlite sqlx driver.
#[derive(Debug)]
pub struct RustqliteError {
    message: String,
    code: i64,
}

impl RustqliteError {
    /// Map an engine error, classifying constraint violations from their
    /// SQLite-exact message prefix (the engine's Display renders them
    /// verbatim, e.g. "UNIQUE constraint failed: uniq.email").
    pub(crate) fn from_engine(err: &crate::error::Error) -> Self {
        let message = err.to_string();
        let (code, _kind) = classify(&message);
        Self { message, code }
    }

    /// `SQLITE_BUSY` — another pool connection owns the write transaction.
    #[allow(dead_code)]
    pub(crate) fn busy() -> Self {
        Self {
            message: "database is locked".into(),
            code: 5, // SQLITE_BUSY
        }
    }

    fn from_message(message: String) -> Self {
        let (code, _kind) = classify(&message);
        Self { message, code }
    }
}

/// Classify a SQLite-style error message into (extended code, kind).
fn classify(message: &str) -> (i64, ErrorKind) {
    // Constraint errors: match on the SQLite message prefix. The engine's
    // Display for Error::Constraint is prefix-free and byte-identical to
    // SQLite, so prefix matching is exact.
    if message.starts_with("UNIQUE constraint failed") {
        return (2067, ErrorKind::UniqueViolation); // SQLITE_CONSTRAINT_UNIQUE
    }
    if message.starts_with("NOT NULL constraint failed") {
        return (1299, ErrorKind::NotNullViolation); // SQLITE_CONSTRAINT_NOTNULL
    }
    if message.starts_with("FOREIGN KEY constraint failed") {
        return (787, ErrorKind::ForeignKeyViolation); // SQLITE_CONSTRAINT_FOREIGNKEY
    }
    if message.starts_with("CHECK constraint failed") {
        return (275, ErrorKind::CheckViolation); // SQLITE_CONSTRAINT_CHECK
    }
    if message.starts_with("database is locked") {
        return (5, ErrorKind::Other); // SQLITE_BUSY
    }
    (1, ErrorKind::Other) // SQLITE_ERROR
}

impl fmt::Display for RustqliteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RustqliteError {}

impl DatabaseError for RustqliteError {
    fn message(&self) -> &str {
        &self.message
    }

    fn code(&self) -> Option<Cow<'_, str>> {
        Some(Cow::Owned(self.code.to_string()))
    }

    fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self
    }

    fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
        self
    }

    fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        self
    }

    fn kind(&self) -> ErrorKind {
        classify(&self.message).1
    }
}

// NOTE: no `From<RustqliteError> for sqlx_core::Error` needed — sqlx-core
// provides a blanket `impl<E: DatabaseError> From<E> for Error`.

/// Convert an engine error into a sqlx error.
pub(crate) fn engine_err(err: crate::error::Error) -> SqlxError {
    SqlxError::Database(Box::new(RustqliteError::from_engine(&err)))
}

/// Build a driver-level error from a message.
pub(crate) fn driver_err(message: impl Into<String>) -> SqlxError {
    SqlxError::Database(Box::new(RustqliteError::from_message(message.into())))
}

/// `SQLITE_BUSY` — the busy timeout expired while another connection's
/// transaction was open.
pub(crate) fn busy() -> SqlxError {
    SqlxError::Database(Box::new(RustqliteError {
        message: "database is locked".into(),
        code: 5, // SQLITE_BUSY
    }))
}

/// Boxed-dynamic error helper for decode failures inside the executor.
#[allow(dead_code)]
pub(crate) fn boxed(msg: impl Into<String>) -> BoxDynError {
    msg.into().into()
}
