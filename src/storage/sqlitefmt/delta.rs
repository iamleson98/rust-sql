//! The SQLite-format session's ROW-DELTA JOURNAL — the capture side
//! of page-level splicing (see `mutator`).
//!
//! The journal accumulates per-table row changes (old + new values in
//! DECLARED column order, exactly what the preupdate event stream
//! carries) between two publishes; the commit layer turns them into
//! page-level mutation ops instead of re-collecting the whole object.
//!
//! EXACTNESS DISCIPLINE — the journal is advisory by construction,
//! never load-bearing for correctness:
//! * per-STATEMENT watermarks: a failed statement's rows are
//!   truncated (the engine undoes the statement, SQLite ABORT/FAIL
//!   semantics);
//! * per-TRANSACTION watermarks: ROLLBACK truncates the open
//!   transaction's rows;
//! * POISON: any write path that does not flow through the journal —
//!   `BEGIN CONCURRENT` / the implicit join, SAVEPOINT / RELEASE /
//!   ROLLBACK TO, DDL, VACUUM, a nested `execute` — clears everything
//!   and suspends capture until the next publish; those commits fall
//!   back to the whole-object splice (today's behavior);
//! * APPLY-TIME VERIFICATION: every mutation op carries the bytes it
//!   expects to find; a stale journal fails the object back to the
//!   whole-object splice (never corruption).
//! * VERSION GUARD: mutations are offered only when the coordinator's
//!   span version matches the session's last-published version (see
//!   `container::ContainerState::span_versions`) — another session's
//!   intervening commit means last-writer-wins whole-splices instead.

use crate::types::Value;
use std::collections::HashMap;

/// One row change, in declared column order.
#[derive(Debug, Clone)]
pub struct RowDelta {
    pub rowid: i64,
    /// Pre-change values (`None` = the row did not exist).
    pub old: Option<Vec<Value>>,
    /// Post-change values (`None` = the row was deleted).
    pub new: Option<Vec<Value>>,
}

/// The journal. All fields are guarded by the caller's mutex.
#[derive(Debug, Default)]
pub struct DeltaJournal {
    /// Lowercased table name → deltas since the last successful
    /// publish, in fire order.
    pub deltas: HashMap<String, Vec<RowDelta>>,
    /// Row counts at the enclosing BEGIN (ROLLBACK truncates to it).
    pub txn_mark: Option<HashMap<String, usize>>,
    /// Row counts at the current statement's start (statement abort
    /// truncates to it).
    pub stmt_mark: HashMap<String, usize>,
    /// Set when a non-journaled write path ran: capture suspends and
    /// the deltas are dropped at the next publish (which rebuilds).
    pub poisoned: bool,
}

impl DeltaJournal {
    /// Take one table's deltas (the publish consumes them).
    pub fn take_table(&mut self, table: &str) -> Option<Vec<RowDelta>> {
        self.deltas.remove(table)
    }

    /// Drop one table's deltas without returning them.
    pub fn drop_table(&mut self, table: &str) {
        self.deltas.remove(table);
    }

    /// Record a row change (fire order).
    pub fn record(
        &mut self,
        table: &str,
        rowid: i64,
        old: Option<Vec<Value>>,
        new: Option<Vec<Value>>,
    ) {
        if self.poisoned {
            return;
        }
        self.deltas
            .entry(table.to_ascii_lowercase())
            .or_default()
            .push(RowDelta { rowid, old, new });
    }

    fn lens(&self) -> HashMap<String, usize> {
        self.deltas
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect()
    }

    fn truncate_to(&mut self, mark: &HashMap<String, usize>) {
        let keys: Vec<String> = self.deltas.keys().cloned().collect();
        for k in keys {
            let keep = mark.get(&k).copied().unwrap_or(0);
            match self.deltas.get_mut(&k) {
                Some(v) if v.len() > keep => v.truncate(keep),
                Some(_) => {}
                None => {}
            }
            if self
                .deltas
                .get(&k)
                .is_some_and(|v| v.is_empty() && !mark.contains_key(&k))
            {
                self.deltas.remove(&k);
            }
        }
    }

    /// Statement boundary bookkeeping.
    pub fn begin_stmt(&mut self) {
        if self.poisoned {
            return;
        }
        self.stmt_mark = self.lens();
    }

    /// A statement failed: its rows never reached the tree.
    pub fn abort_stmt(&mut self) {
        if self.poisoned {
            return;
        }
        self.truncate_to(&self.stmt_mark.clone());
    }

    /// BEGIN: remember the transaction watermark.
    pub fn begin_txn(&mut self) {
        if self.poisoned {
            return;
        }
        if self.txn_mark.is_none() {
            self.txn_mark = Some(self.lens());
        }
    }

    /// COMMIT: the transaction's rows are durable-in-memory.
    pub fn commit_txn(&mut self) {
        self.txn_mark = None;
    }

    /// ROLLBACK: the transaction's rows never happened.
    pub fn rollback_txn(&mut self) {
        if let Some(mark) = self.txn_mark.take() {
            self.truncate_to(&mark);
        }
    }

    /// A non-journaled write path ran: drop everything and suspend
    /// capture until the next publish.
    pub fn poison(&mut self) {
        self.deltas.clear();
        self.txn_mark = None;
        self.stmt_mark.clear();
        self.poisoned = true;
    }

    /// The publish consumed (or rebuilt past) the journal.
    pub fn reset_after_publish(&mut self) {
        self.deltas.clear();
        self.txn_mark = None;
        self.stmt_mark.clear();
        self.poisoned = false;
    }

    /// Whether capture is currently active.
    pub fn active(&self) -> bool {
        !self.poisoned
    }
}
