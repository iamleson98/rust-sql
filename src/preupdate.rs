//! Row-level pre-change events — the engine side of SQLite's preupdate
//! hook family (`sqlite3_preupdate_hook` / `_old` / `_new` / `_count` /
//! `_depth`).
//!
//! A hook is a closure invoked BEFORE each row is written, with the row's
//! pre-change (`old`) and post-change (`new`) column values — richer than
//! the post-change `sqlite3_update_hook`, and usable to implement
//! change-data-capture, audit ledgers, and the session/patchset family.
//!
//! Public surface:
//! - [`crate::Database::set_preupdate_hook`] registers the hook.
//! - [`PreupdateEvent`] carries one row change; see its fields for the
//!   exact SQLite-matched semantics (rowid=0 for WITHOUT ROWID tables,
//!   `depth` counting trigger/FK-action nesting, values in declared
//!   column order).
//!
//! Firing plumbing: the executor's write sites call [`fire`], which is a
//! TLS-flag check and nothing more when no hook is installed (the
//! no-hook cost is one thread-local load — the hot insert/append paths
//! stay allocation-free). `Database::execute` installs the hook into the
//! thread-local sink for the duration of the statement; the C-ABI
//! compatibility layer installs its own connection-scoped sink around
//! the shared engine's writes (per-connection semantics with one engine
//! per file).
//!
//! Depth: 0 for rows changed directly by the top-level statement, +1 per
//! trigger-body or FK-action nesting level (SQLite's preupdate depth).

use crate::schema::Table;
use crate::types::Value;
use std::cell::{Cell, RefCell};

/// Which change is about to happen to the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreupdateOp {
    Insert,
    Delete,
    Update,
}

impl PreupdateOp {
    /// SQLite's callback op codes: SQLITE_INSERT=18, SQLITE_DELETE=9,
    /// SQLITE_UPDATE=23.
    pub fn code(self) -> i32 {
        match self {
            PreupdateOp::Insert => 18,
            PreupdateOp::Delete => 9,
            PreupdateOp::Update => 23,
        }
    }
}

/// One row change, fired BEFORE the row is written.
#[derive(Debug)]
pub struct PreupdateEvent {
    pub op: PreupdateOp,
    /// Always `"main"` (a single attached database).
    pub db: &'static str,
    /// Table name as declared (SQLite passes the exact stored name).
    pub table: String,
    /// The row's rowid. WITHOUT ROWID tables report 0 — SQLite's
    /// observable value for them (the PK is not exposed here).
    pub rowid: i64,
    /// Pre-change column values in DECLARED order (Delete / Update).
    pub old: Option<Vec<Value>>,
    /// Post-change column values in DECLARED order (Insert / Update).
    pub new: Option<Vec<Value>>,
    /// 0 = direct statement change; +1 per trigger body / FK-action
    /// nesting level.
    pub depth: u32,
}

/// User-visible hook type: `FnMut(&PreupdateEvent) + Send`.
pub type PreupdateHook = Box<dyn FnMut(&PreupdateEvent) + Send>;

/// Where a `Database` stores its hook. A plain mutex over the slot:
/// `set_preupdate_hook` takes `&mut Database` (mutation is exclusive with
/// every execution), and `fire` locks per event so two threads stepping
/// statements on one shared `&Database` never race the closure.
pub type HookSlot = std::sync::Mutex<Option<PreupdateHook>>;

/// What the thread-local sink holds: either a pointer to a `Database`'s
/// hook slot (installed by `Database::execute` / statement stepping for
/// the duration of one statement — the Database outlives the statement,
/// so the pointer stays valid), or an owned closure installed by the
/// C-ABI layer (connection-scoped, wraps the raw callback). The `db`
/// field is the owning Database's address — nested-execute detection.
enum Sink {
    /// Borrow of a Database's hook slot + the Database's identity.
    Borrowed { mutex: *const HookSlot, db: usize },
    /// Owned closure (compat layer: captures the connection).
    Owned(Box<dyn FnMut(&PreupdateEvent) + Send>),
}

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// True when the active borrowed sink points at the given hook slot —
/// used to detect a nested `execute` on the SAME Database (skip
/// re-installing, which would re-lock the slot's mutex and deadlock).
#[inline]
pub(crate) fn sink_for_db(db: usize) -> bool {
    SINK.with(|s| matches!(s.borrow().as_ref(), Some(Sink::Borrowed { db: d, .. }) if *d == db))
}

/// Pops the sink / restores the previous one on drop (unwind-safe).
pub struct SinkGuard {
    prev: Option<Sink>,
}

impl Drop for SinkGuard {
    fn drop(&mut self) {
        SINK.with(|s| *s.borrow_mut() = self.prev.take());
    }
}

/// Install a Database's hook slot (by mutex pointer) as this thread's
/// firing sink. `db` is the owning Database's address (identity for
/// nested-execute detection — the nested call sees the sink pointing at
/// itself and skips). The caller checked the slot is non-empty.
pub(crate) fn install_borrowed(mutex: *const HookSlot, db: usize) -> SinkGuard {
    let prev = SINK.with(|s| (*s.borrow_mut()).replace(Sink::Borrowed { mutex, db }));
    SinkGuard { prev }
}

/// Install an owned closure as this thread's firing sink (the C-ABI
/// compatibility layer's per-connection bridge). Public: the compat
/// crate is a separate crate.
pub fn install_owned(hook: Box<dyn FnMut(&PreupdateEvent) + Send>) -> SinkGuard {
    let prev = SINK.with(|s| (*s.borrow_mut()).replace(Sink::Owned(hook)));
    SinkGuard { prev }
}

/// True when a sink is installed — the write sites' one-TLS-load
/// hot-path check.
#[inline]
pub(crate) fn hook_installed() -> bool {
    SINK.with(|s| s.borrow().is_some())
}

/// Bump the nesting depth for the duration of a trigger body or an FK
/// action application (drop restores).
pub(crate) struct DepthGuard;

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

pub(crate) fn enter_nested_change() -> DepthGuard {
    DEPTH.with(|d| d.set(d.get() + 1));
    DepthGuard
}

/// Fire one row-change event. No-op (one TLS load) when no hook is
/// installed; otherwise materializes the event and calls the hook.
///
/// `rowid` is the engine's internal rowid; WITHOUT ROWID tables report
/// 0 (SQLite's observable behavior).
pub(crate) fn fire(
    op: PreupdateOp,
    table: &Table,
    rowid: i64,
    old: Option<&[Value]>,
    new: Option<&[Value]>,
) {
    if !hook_installed() {
        return;
    }
    let event = PreupdateEvent {
        op,
        db: "main",
        table: table.name.clone(),
        rowid: if table.without_rowid { 0 } else { rowid },
        old: old.map(|v| v.to_vec()),
        new: new.map(|v| v.to_vec()),
        depth: DEPTH.with(|d| d.get()),
    };
    SINK.with(|s| {
        let mut slot = s.borrow_mut();
        match slot.as_mut() {
            Some(Sink::Borrowed { mutex, .. }) => {
                // Copy the raw pointer out (match ergonomics binds it by
                // reference).
                let mutex: *const HookSlot = *mutex;
                // SAFETY: the Mutex lives in a Database that outlives the
                // statement; `lock()` is `&self`, and the closure runs
                // under the mutex's own synchronization (two threads
                // stepping statements on one shared &Database are
                // serialized here).
                let Ok(mut guard) = (unsafe { (*mutex).lock() }) else {
                    return;
                };
                if let Some(h) = guard.as_mut() {
                    h(&event);
                }
            }
            Some(Sink::Owned(h)) => h(&event),
            None => {}
        }
    });
}

/// Fire a DELETE event when only the row's encoded payload is at hand:
/// decodes the old values first (cold path — runs only when a hook is
/// installed).
pub(crate) fn fire_delete_payload(table: &Table, rowid: i64, payload: &[u8]) {
    if !hook_installed() {
        return;
    }
    if let Ok(old_row) =
        crate::storage::row_codec::decode_row(payload, table.n_columns(), rowid, table.rowid_alias)
    {
        fire(PreupdateOp::Delete, table, rowid, Some(&old_row), None);
    }
}

/// True while a hook is installed — exported for the streaming paths
/// that need to decide whether to keep old payloads around.
#[inline]
pub fn enabled() -> bool {
    hook_installed()
}
