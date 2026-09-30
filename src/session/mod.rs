//! The session extension — SQLite's `sqlite3session` /
//! `sqlite3changeset` / `sqlite3changegroup` / `sqlite3rebaser` family,
//! transliterated from sqlite3session.c and pinned against the bundled
//! real-SQLite oracle (see `tests/session_differential.rs`).
//!
//! A [`Session`] records row changes made through its connection (the
//! engine's preupdate event stream — the same stream the preupdate hook
//! and the SQLite-format delta journal consume) and produces
//! byte-compatible **changesets** and **patchsets**:
//!
//! * [`Session::changeset`] / [`Session::patchset`] — the binary
//!   formats SQLite documents (table-grouped, one record per change);
//! * [`changeset_apply`] — apply a changeset/patchset with a conflict
//!   handler (DATA / NOTFOUND / CONFLICT / CONSTRAINT / FOREIGN_KEY;
//!   OMIT / REPLACE / ABORT resolutions, deferred-constraint retries,
//!   rebase-blob production);
//! * [`changeset_invert`] / [`changeset_concat`] / [`ChangeGroup`] —
//!   buffer-level transforms;
//! * [`Rebaser`] — rebase a local changeset against a remote node's
//!   OMIT/REPLACE resolutions (SQLite's begin-concurrent workflow).
//!
//! Ordering discipline (byte-identical output vs SQLite): tables in
//! first-attached order; per-table changes in hash-bucket order (256
//! buckets first, doubling at half load, prepend + re-hash-reverse);
//! capture keeps the FIRST change per row key; generation resolves
//! entries against the CURRENT database state (self-healing for
//! rolled-back statements).
//!
//! Per-connection semantics: sessions register under the current
//! connection identity ([`crate::ConnIdentityGuard`] — the C-ABI layer
//! arms it per handle), so one engine shared between handles keeps
//! each connection's session to its own writes, exactly like SQLite.

pub mod apply;
pub mod codec;
pub mod gen;
pub(crate) mod group;
pub mod iter;
pub(crate) mod rebase;
pub(crate) mod state;
pub(crate) mod transform;

pub use apply::{
    apply as changeset_apply, ApplyError, ConflictEvent, ConflictVerdict, APPLY_FKNOACTION,
    APPLY_IGNORENOOP, APPLY_INVERT, APPLY_NOSAVEPOINT, APPLY_REBASE, CHANGESET_ABORT,
    CHANGESET_CONFLICT, CHANGESET_CONSTRAINT, CHANGESET_DATA, CHANGESET_FOREIGN_KEY,
    CHANGESET_NOTFOUND, CHANGESET_OMIT, CHANGESET_REPLACE,
};
pub use codec::SessVal;
pub use codec::{OP_DELETE, OP_INSERT, OP_UPDATE};
pub use group::{concat as changeset_concat, ChangeGroup};
pub use iter::ChangesetIter;
pub use rebase::Rebaser;
pub use state::{SessionCore, SessionError};
pub use transform::invert as changeset_invert;

use crate::api::Database;
use crate::error::Error;
use std::sync::{Arc, Mutex, Weak};

/// A session handle — records changes until asked for a changeset.
/// Create through [`Database::create_session`].
///
/// A fresh session records NOTHING until [`Session::attach`] (NULL for
/// every table) or [`Session::set_table_filter`] — SQLite's default.
#[derive(Clone)]
pub struct Session {
    core: Arc<Mutex<SessionCore>>,
}

// The C-ABI layer holds sessions across threads (one per connection).
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

impl Session {
    pub(crate) fn from_core(core: Arc<Mutex<SessionCore>>) -> Self {
        Session { core }
    }

    /// The shared core (the engine registry + the C-ABI layer store
    /// these).
    pub fn core(&self) -> &Arc<Mutex<SessionCore>> {
        &self.core
    }

    /// `sqlite3session_enable` — a disabled session records nothing.
    pub fn enable(&self, on: bool) {
        self.core.lock().unwrap().enable = on;
    }

    /// `sqlite3session_is_enabled` (the inverse of the C macro shape).
    pub fn is_enabled(&self) -> bool {
        self.core.lock().unwrap().enable
    }

    /// `sqlite3session_indirect` — changes recorded while indirect are
    /// marked in the changeset (the `bIndirect` byte).
    pub fn set_indirect(&self, on: bool) {
        self.core.lock().unwrap().indirect = on;
    }

    pub fn is_indirect(&self) -> bool {
        self.core.lock().unwrap().indirect
    }

    /// `sqlite3session_attach`: `None` → every table (including tables
    /// created later); `Some(name)` → one table (idempotent, order
    /// preserving — the attach order is the changeset's table order).
    pub fn attach(&self, table: Option<&str>) {
        self.core.lock().unwrap().attach(table);
    }

    /// `sqlite3session_table_filter` — auto-attach through a predicate.
    pub fn set_table_filter<F>(&self, f: Option<F>)
    where
        F: Fn(&str) -> bool + Send + 'static,
    {
        let f = f.map(|f| Box::new(f) as Box<dyn Fn(&str) -> bool + Send>);
        self.core.lock().unwrap().set_table_filter(f);
    }

    /// `sqlite3session_isempty`.
    pub fn is_empty(&self) -> bool {
        self.core.lock().unwrap().is_empty()
    }

    /// SQLITE_SESSION_OBJCONFIG_ROWID: track tables with no explicit
    /// PRIMARY KEY through a synthetic `_rowid_ INTEGER PRIMARY KEY`
    /// column. OFF by default (SQLite: such tables are ignored). Must
    /// be set before the first attach (a misuse is ignored here — the
    /// C ABI reports it).
    pub fn set_implicit_rowid_pk(&self, on: bool) -> bool {
        let mut core = self.core.lock().unwrap();
        if core.tables.is_empty() {
            core.implicit_pk = on;
        }
        core.implicit_pk
    }

    pub fn implicit_rowid_pk(&self) -> bool {
        self.core.lock().unwrap().implicit_pk
    }

    /// A byte estimate of the pending changeset
    /// (`sqlite3session_changeset_size`; requires the size flag).
    pub fn changeset_size(&self) -> usize {
        self.core.lock().unwrap().changeset_size()
    }

    /// Number of recorded changes (introspection).
    pub fn change_count(&self) -> usize {
        self.core.lock().unwrap().change_count()
    }

    /// `sqlite3session_changeset` — resolve the recorded changes against
    /// `db`'s CURRENT state into a binary changeset.
    pub fn changeset(&self, db: &Database) -> Result<Vec<u8>, Error> {
        let mut core = self.core.lock().unwrap();
        gen::generate(&mut core, db, false)
    }

    /// `sqlite3session_patchset`.
    pub fn patchset(&self, db: &Database) -> Result<Vec<u8>, Error> {
        let mut core = self.core.lock().unwrap();
        gen::generate(&mut core, db, true)
    }

    /// The sticky session error (SQLITE_SCHEMA-class drift), if any.
    pub fn error(&self) -> Option<String> {
        self.core.lock().unwrap().rc.as_ref().map(|e| e.0.clone())
    }

    /// Clear the sticky error (`sqlite3session_create`'s fresh start).
    pub fn clear_error(&self) {
        self.core.lock().unwrap().rc = None;
    }
}

/// Build a one-change changeset from parsed parts — the C-ABI layer's
/// conflict mirror (the handler's iterator must honestly serve
/// op/old/new for the event it received).
pub fn synthetic_change(
    table: &str,
    n_col: usize,
    pk: &[u8],
    op: u8,
    indirect: bool,
    old: &[Option<SessVal>],
    new: &[Option<SessVal>],
) -> Vec<u8> {
    let mut cs = Vec::new();
    cs.push(b'T');
    codec::put_varint(&mut cs, n_col as u64);
    cs.extend_from_slice(pk);
    cs.extend_from_slice(table.as_bytes());
    cs.push(0);
    cs.push(op);
    cs.push(indirect as u8);
    for side in [old, new] {
        for i in 0..n_col {
            let v = side.get(i).and_then(|v| v.as_ref());
            let mut tmp = Vec::new();
            match v {
                Some(SessVal::Null) => tmp.push(0x05),
                Some(sv) => match sv.to_value() {
                    Some(val) => codec::append_value(&mut tmp, Some(&val)),
                    None => tmp.push(0x00),
                },
                None => tmp.push(0x00),
            }
            cs.extend_from_slice(&tmp);
        }
    }
    cs
}

/// Registration bookkeeping shared between the Database and its
/// sessions: (connection id, weak handle) pairs. Dead handles are
/// pruned at each statement arming.
pub(crate) type SessionRegistry = Vec<(u64, Weak<Mutex<SessionCore>>)>;

/// Collect the live session handles registered for `conn` out of the
/// registry, pruning dead entries.
pub(crate) fn prune_and_collect(
    registry: &mut SessionRegistry,
    conn: u64,
) -> Vec<Arc<Mutex<SessionCore>>> {
    let mut out = Vec::new();
    registry.retain(|(id, w)| {
        if *id == conn {
            match w.upgrade() {
                Some(h) => {
                    out.push(h);
                    true
                }
                None => false, // dead session — prune
            }
        } else {
            true // another connection's session — keep
        }
    });
    out
}
