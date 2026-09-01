//! Unified error type for the entire database engine.
//!
//! All public APIs return `Result<T, RustqliteError>`. Internal modules
//! convert their specific error types into this enum via `?` + `From` impls.

use std::fmt;
use std::io;

/// The single error type used across the engine.
#[derive(Debug)]
pub enum Error {
    /// I/O error from the underlying file system.
    Io(io::Error),
    /// A page-level corruption or invariant violation in the storage layer.
    Corruption(String),
    /// The B+tree could not insert or delete because of an internal bug.
    Btree(String),
    /// WAL checksum mismatch or frame ordering problem.
    Wal(String),
    /// MVCC conflict: a transaction tried to read a page that has been
    /// reclaimed, or two writers collided.
    Transaction(String),
    /// SQL lexer error (bad token, unterminated string, etc.).
    Lex { line: usize, col: usize, msg: String },
    /// SQL parser error (unexpected token, malformed syntax).
    Parse { line: usize, col: usize, msg: String },
    /// SQL semantic error (unknown column, type mismatch, ambiguous name).
    Semantic(String),
    /// Constraint violation with a SQLite-exact message. The Display is
    /// deliberately prefix-free: SQLite's `sqlite3_errmsg` renders these
    /// verbatim ("UNIQUE constraint failed: t.c", "NOT NULL constraint
    /// failed: t.c", "CHECK constraint failed: t", "FOREIGN KEY
    /// constraint failed") and ORMs (sqlx, sea-orm) pattern-match on the
    /// exact bytes. Keep the message content byte-identical to SQLite.
    Constraint(String),
    /// Query planner could not produce a valid plan.
    Planner(String),
    /// Runtime execution error (division by zero, type coercion failure).
    Runtime(String),
    /// Caller asked for something that is not yet implemented.
    Unsupported(&'static str),
    /// Schema object does not exist.
    NotFound(String),
    /// Schema object already exists.
    AlreadyExists(String),
    /// Invalid argument from the user.
    InvalidArgument(String),
}

impl Error {
    pub fn lex(line: usize, col: usize, msg: impl Into<String>) -> Self {
        Error::Lex { line, col, msg: msg.into() }
    }
    pub fn parse(line: usize, col: usize, msg: impl Into<String>) -> Self {
        Error::Parse { line, col, msg: msg.into() }
    }
    pub fn corruption(msg: impl Into<String>) -> Self {
        Error::Corruption(msg.into())
    }
    pub fn semantic(msg: impl Into<String>) -> Self {
        Error::Semantic(msg.into())
    }
    /// SQLite-exact constraint violation (see [`Error::Constraint`]).
    pub fn constraint(msg: impl Into<String>) -> Self {
        Error::Constraint(msg.into())
    }
    pub fn runtime(msg: impl Into<String>) -> Self {
        Error::Runtime(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {}", e),
            Error::Corruption(m) => write!(f, "corruption: {}", m),
            Error::Btree(m) => write!(f, "btree: {}", m),
            Error::Wal(m) => write!(f, "wal: {}", m),
            Error::Transaction(m) => write!(f, "transaction: {}", m),
            Error::Lex { line, col, msg } => write!(f, "lex error at {}:{}: {}", line, col, msg),
            Error::Parse { line, col, msg } => write!(f, "parse error at {}:{}: {}", line, col, msg),
            Error::Semantic(m) => write!(f, "semantic error: {}", m),
            // Prefix-free: byte-identical to SQLite's errmsg.
            Error::Constraint(m) => write!(f, "{}", m),
            Error::Planner(m) => write!(f, "planner: {}", m),
            Error::Runtime(m) => write!(f, "runtime error: {}", m),
            Error::Unsupported(m) => write!(f, "unsupported: {}", m),
            Error::NotFound(m) => write!(f, "not found: {}", m),
            Error::AlreadyExists(m) => write!(f, "already exists: {}", m),
            Error::InvalidArgument(m) => write!(f, "invalid argument: {}", m),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<std::num::TryFromIntError> for Error {
    fn from(e: std::num::TryFromIntError) -> Self {
        Error::Corruption(format!("integer conversion: {}", e))
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(e: std::str::Utf8Error) -> Self {
        Error::Corruption(format!("utf8: {}", e))
    }
}

impl From<std::array::TryFromSliceError> for Error {
    fn from(e: std::array::TryFromSliceError) -> Self {
        Error::Corruption(format!("slice conversion: {}", e))
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;
