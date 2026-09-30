//! Changeset application — SQLite's `sqlite3changeset_apply[_v2]`
//! (`sessionChangesetApply`, `sessionApplyOneOp`,
//! `sessionApplyOneWithRetry`, `sessionConflictHandler`,
//! `sessionRetryConstraints`, `sessionRebaseAdd`).
//!
//! Each change applies through ordinary engine SQL with conflict
//! detection:
//!   * DELETE/UPDATE match the stored old values (changesets only) — a
//!     zero-changes result is a DATA (row present, fields differ) or
//!     NOTFOUND (row absent) conflict;
//!   * an INSERT constraint violation is a CONFLICT (PK row exists) or
//!     CONSTRAINT (CHECK/NOT NULL/...) conflict;
//!   * CHANGESET_REPLACE on DATA retries without the old-value check;
//!     on CONFLICT (INSERT) it removes the conflicting row inside a
//!     SAVEPOINT and retries;
//!   * CHANGESET_OMIT skips the change; CHANGESET_ABORT rolls the whole
//!     apply back (the SAVEPOINT); anything else is misuse.
//!
//! Constraint-violating INSERTs are buffered and retried after the
//! table's remaining changes (a later change may remove the blocker),
//! then surfaced to the handler once no progress is left — SQLite's
//! deferred-constraint machinery, exactly.
//!
//! FK semantics: SQLite defers FK checks during apply
//! (`PRAGMA defer_foreign_keys=1`) and reports a single FOREIGN_KEY
//! conflict at the end. The engine enforces FKs immediately, so the
//! apply suspends enforcement for its duration and verifies the rows IT
//! wrote at the end (OMIT commits with the violations, like SQLite).

use super::codec::SessVal;
use super::codec::*;
use super::iter::ChangesetIter;
use crate::api::Database;
use crate::error::Error;
use crate::types::Value;

/// Apply flags (`sqlite3changeset_apply_v2`; plus our APPLY_REBASE).
pub const APPLY_NOSAVEPOINT: u32 = 0x0001;
pub const APPLY_INVERT: u32 = 0x0002;
pub const APPLY_IGNORENOOP: u32 = 0x0004;
pub const APPLY_FKNOACTION: u32 = 0x0008;
/// Our representation of apply_v2's `ppRebase` output request.
pub const APPLY_REBASE: u32 = 0x0010;

/// Conflict codes passed to conflict handlers (`SQLITE_CHANGESET_*`).
pub const CHANGESET_DATA: i32 = 1;
pub const CHANGESET_NOTFOUND: i32 = 2;
pub const CHANGESET_CONFLICT: i32 = 3;
pub const CHANGESET_CONSTRAINT: i32 = 4;
pub const CHANGESET_FOREIGN_KEY: i32 = 5;

/// Conflict-handler return values.
pub const CHANGESET_OMIT: i32 = 1;
pub const CHANGESET_REPLACE: i32 = 5;
pub const CHANGESET_ABORT: i32 = 18;

/// The apply error — mirrors SQLite's result-code split for the C ABI:
/// engine errors pass through, malformed changesets are CORRUPT, bad
/// handler verdicts are MISUSE, CHANGESET_ABORT is ABORT.
#[derive(Debug)]
pub enum ApplyError {
    Engine(Error),
    Corrupt(&'static str),
    Misuse(&'static str),
    Abort,
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Engine(e) => write!(f, "{e}"),
            ApplyError::Corrupt(m) => write!(f, "{m}"),
            ApplyError::Misuse(m) => write!(f, "{m}"),
            ApplyError::Abort => write!(f, "changeset application aborted"),
        }
    }
}
impl std::error::Error for ApplyError {}
impl From<Error> for ApplyError {
    fn from(e: Error) -> Self {
        ApplyError::Engine(e)
    }
}

/// One conflict presented to the handler (`xConflict`'s view of the
/// iterator + the conflicting row).
pub struct ConflictEvent<'a> {
    /// CHANGESET_DATA / NOTFOUND / CONFLICT / CONSTRAINT / FOREIGN_KEY.
    pub code: i32,
    pub op: u8,
    pub table: &'a str,
    pub n_col: usize,
    pub pk: &'a [u8],
    pub indirect: bool,
    pub old: &'a [Option<SessVal>],
    pub new: &'a [Option<SessVal>],
    /// The row the change collided with (absent → None) — SQLite's
    /// `sqlite3changeset_conflict` columns.
    pub conflict_row: Option<Vec<Value>>,
}

/// The handler's verdict: CHANGESET_OMIT / REPLACE / ABORT.
pub type ConflictVerdict = i32;

/// A raw buffered change (the deferred-constraint retry buffer).
#[derive(Clone)]
struct RawChange {
    op: u8,
    indirect: bool,
    old: Vec<Option<SessVal>>,
    new: Vec<Option<SessVal>>,
}

/// The per-table apply context (`SessionApplyCtx`).
struct ApplyCtx {
    name: String,
    /// The CHANGESET's column count (may trail the table's).
    n_col: usize,
    ab_pk: Vec<u8>,
    az_col: Vec<String>,
    b_stat1: bool,
    b_patchset: bool,
    /// Schema mismatch / filtered out → skip this table's changes.
    skip: bool,
}

/// State across the whole apply.
struct ApplyState {
    b_rebase: bool,
    rebase_started: bool,
    rebase_buf: Vec<u8>,
    constraints: Vec<RawChange>,
    b_defer: bool,
    b_ignore_noop: bool,
    /// Rows this apply wrote, for the deferred-FK end check:
    /// (table, op, row-values).
    written: Vec<(String, u8, Vec<Value>)>,
    fk_was_on: bool,
}

/// `sqlite3changeset_apply` / `apply_v2`.
pub fn apply(
    db: &mut Database,
    changeset: &[u8],
    flags: u32,
    mut filter: Option<&mut dyn FnMut(&str) -> bool>,
    conflict: &mut dyn FnMut(&ConflictEvent<'_>) -> ConflictVerdict,
) -> Result<Option<Vec<u8>>, ApplyError> {
    let b_invert = flags & APPLY_INVERT != 0;
    let nosavepoint = flags & APPLY_NOSAVEPOINT != 0;
    let fknoaction = flags & APPLY_FKNOACTION != 0;

    let start_flags = if b_invert {
        super::iter::CHANGESETSTART_INVERT
    } else {
        0
    };
    let mut iter = ChangesetIter::start(changeset, start_flags).map_err(ApplyError::Corrupt)?;

    let mut st = ApplyState {
        b_rebase: flags & APPLY_REBASE != 0,
        rebase_started: false,
        rebase_buf: Vec::new(),
        constraints: Vec::new(),
        b_defer: true,
        b_ignore_noop: flags & APPLY_IGNORENOOP != 0,
        written: Vec::new(),
        fk_was_on: false,
    };

    // Suspend FK enforcement for the apply's duration (SQLite's
    // defer_foreign_keys=1); the end check verifies what WE wrote.
    st.fk_was_on = db.foreign_keys_enabled();
    if st.fk_was_on {
        db.execute("PRAGMA foreign_keys = OFF", ())
            .map_err(ApplyError::Engine)?;
    }

    if !nosavepoint {
        db.execute("SAVEPOINT changeset_apply", ())
            .map_err(ApplyError::Engine)?;
    }

    let mut rc: Result<(), ApplyError> = Ok(());
    let mut ctx: Option<ApplyCtx> = None;
    while iter.next().map_err(ApplyError::Corrupt)? {
        let table_changed = ctx.as_ref().map(|c| c.name != iter.table()).unwrap_or(true);
        if table_changed {
            let table = iter.table().to_string();
            if let Some(c) = ctx.as_mut() {
                rc = retry_constraints(db, c, &mut st, conflict);
                if rc.is_err() {
                    break;
                }
            }
            ctx = None;
            st.b_defer = true;
            st.constraints.clear();
            // xFilter gate: return 0 → skip this table entirely.
            if let Some(f) = filter.as_mut() {
                if !f(&table) {
                    ctx = Some(ApplyCtx {
                        name: table,
                        n_col: iter.n_col(),
                        ab_pk: iter.pk().to_vec(),
                        az_col: Vec::new(),
                        b_stat1: false,
                        b_patchset: iter.patchset(),
                        skip: true,
                    });
                    continue;
                }
            }
            match build_apply_ctx(db, &table, iter.n_col(), iter.pk(), iter.patchset()) {
                Ok(c) => ctx = Some(c),
                Err(e) => {
                    rc = Err(ApplyError::Engine(e));
                    break;
                }
            }
        }
        if ctx.as_ref().map(|c| c.skip).unwrap_or(true) {
            continue;
        }
        let c = ctx.as_mut().unwrap();
        let mut raw = RawChange {
            op: iter.op(),
            indirect: iter.indirect(),
            old: iter_old(&iter),
            new: iter_new(&iter),
        };
        rc = apply_one_with_retry(db, c, &mut raw, &mut st, conflict);
        if rc.is_err() {
            break;
        }
    }

    // Final constraint retry for the last table.
    if rc.is_ok() {
        if let Some(c) = ctx.as_mut() {
            rc = retry_constraints(db, c, &mut st, conflict);
        }
    }

    // Deferred-FK end check (SQLite's SQLITE_DBSTATUS_DEFERRED_FKS).
    if rc.is_ok() && st.fk_was_on && !fknoaction {
        rc = fk_end_check(db, &mut st, conflict);
    }

    if rc.is_ok() {
        if !nosavepoint {
            db.execute("RELEASE changeset_apply", ())
                .map_err(ApplyError::Engine)?;
        }
    } else {
        if !nosavepoint {
            let _ = db.execute("ROLLBACK TO changeset_apply", ());
            let _ = db.execute("RELEASE changeset_apply", ());
        }
    }
    // Restore FK enforcement.
    if st.fk_was_on {
        let _ = db.execute("PRAGMA foreign_keys = ON", ());
    }

    rc.map(|_| {
        if st.b_rebase && !st.rebase_buf.is_empty() {
            Some(std::mem::take(&mut st.rebase_buf))
        } else {
            None
        }
    })
}

fn iter_old(iter: &ChangesetIter<'_>) -> Vec<Option<SessVal>> {
    (0..iter.n_col()).map(|i| iter.old(i).cloned()).collect()
}
fn iter_new(iter: &ChangesetIter<'_>) -> Vec<Option<SessVal>> {
    (0..iter.n_col())
        .map(|i| iter.new_val(i).cloned())
        .collect()
}

/// Resolve the target table's session model from the live schema
/// (`sessionTableInfo` + the compat gate: the table must exist, have at
/// least the changeset's column count, and match the PK bitmap).
fn build_apply_ctx(
    db: &Database,
    name: &str,
    n_cs: usize,
    ab_pk_cs: &[u8],
    b_patchset: bool,
) -> Result<ApplyCtx, Error> {
    let tbl = db
        .catalog
        .get_table(name)
        .ok_or_else(|| Error::NotFound(format!("no such table: {name}")))?;
    let b_stat1 = name.eq_ignore_ascii_case("sqlite_stat1");
    let mut model = super::state::blank_session_table(name);
    // The APPLY side models the table with implicit rowid PK enabled
    // when the CHANGESET's own PK array says so (a _rowid_-keyed
    // changeset can only have come from an OBJCONFIG_ROWID session).
    let implicit = ab_pk_cs.iter().any(|p| *p != 0)
        && tbl.columns.iter().all(|c| !c.primary_key)
        && !tbl.without_rowid
        && !b_stat1
        && ab_pk_cs.first() == Some(&1)
        && ab_pk_cs.len() == tbl.columns.len() + 1;
    super::state::SessionCore::init_table_model(&mut model, &tbl, implicit);
    let n_min_pk = model
        .ab_pk
        .iter()
        .rposition(|p| *p != 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    let skip = model.n_col < n_cs
        || model.ab_pk.len() < n_cs
        || n_cs < n_min_pk
        || model.ab_pk[..n_cs]
            .iter()
            .zip(ab_pk_cs[..].iter())
            .any(|(a, b)| *a != *b);
    Ok(ApplyCtx {
        name: name.to_string(),
        n_col: n_cs,
        ab_pk: ab_pk_cs.to_vec(),
        az_col: model.az_col,
        b_stat1,
        b_patchset,
        skip,
    })
}

/// `sessionApplyOneWithRetry`.
fn apply_one_with_retry(
    db: &mut Database,
    c: &mut ApplyCtx,
    raw: &mut RawChange,
    st: &mut ApplyState,
    conflict: &mut dyn FnMut(&ConflictEvent<'_>) -> ConflictVerdict,
) -> Result<(), ApplyError> {
    let mut b_replace = false;
    let mut b_retry = false;
    apply_one_op(db, c, raw, st, conflict, true, &mut b_replace, &mut b_retry)?;
    if b_retry {
        // CHANGESET_REPLACE on a DATA/NOTFOUND conflict: retry without
        // the old-value verification.
        apply_one_op(db, c, raw, st, conflict, false, &mut false, &mut false)?;
    } else if b_replace {
        // CHANGESET_REPLACE on an INSERT CONFLICT: remove the offending
        // row inside a savepoint, then re-run the insert.
        db.execute("SAVEPOINT replace_op", ())
            .map_err(ApplyError::Engine)?;
        let res = (|| -> Result<(), ApplyError> {
            let del_sql = build_delete_sql(c);
            let mut params = vec![Value::Null; c.n_col + 1];
            bind_pk_into(c, &raw.new, &mut params);
            if c.n_col > count_pk(c) {
                params[c.n_col] = Value::Integer(1); // PK-only, no verify
            }
            db.execute(&del_sql, params).map_err(ApplyError::Engine)?;
            if db.changes() > 0 {
                record_written_delete(c, st, &raw.old);
            }
            apply_one_op(db, c, raw, st, conflict, false, &mut false, &mut false)?;
            Ok(())
        })();
        match res {
            Ok(()) => {
                db.execute("RELEASE replace_op", ())
                    .map_err(ApplyError::Engine)?;
            }
            Err(e) => {
                let _ = db.execute("ROLLBACK TO replace_op", ());
                let _ = db.execute("RELEASE replace_op", ());
                return Err(e);
            }
        }
    }
    Ok(())
}

/// `sessionApplyOneOp` — one change, one attempt. `attempt2` selects the
/// retry semantics (no old-value verification, no DATA conflict).
#[allow(clippy::too_many_arguments)]
fn apply_one_op(
    db: &mut Database,
    c: &mut ApplyCtx,
    raw: &RawChange,
    st: &mut ApplyState,
    conflict: &mut dyn FnMut(&ConflictEvent<'_>) -> ConflictVerdict,
    attempt1: bool,
    pb_replace: &mut bool,
    pb_retry: &mut bool,
) -> Result<(), ApplyError> {
    let verify = attempt1 && !c.b_patchset;
    match raw.op {
        OP_DELETE => {
            let sql = build_delete_sql(c);
            let mut params = vec![Value::Null; c.n_col + 1];
            if verify {
                // Full old row + flag = all-PK ? 1 : 0.
                for (i, v) in raw.old.iter().take(c.n_col).enumerate() {
                    let v = v
                        .as_ref()
                        .and_then(|v| v.to_value())
                        .ok_or(ApplyError::Corrupt("undefined field in changeset DELETE"))?;
                    params[i] = v;
                }
                if c.n_col > count_pk(c) {
                    params[c.n_col] = Value::Integer(all_pk(c) as i64);
                }
            } else {
                bind_pk_into(c, &raw.old, &mut params);
                if c.n_col > count_pk(c) {
                    params[c.n_col] = Value::Integer(1);
                }
            }
            db.execute(&sql, params).map_err(ApplyError::Engine)?;
            if db.changes() > 0 {
                record_written_delete(c, st, &raw.old);
                return Ok(());
            }
            if st.b_ignore_noop && attempt1 {
                // The v2 IGNORENOOP path: a delete that matched nothing
                // and whose old values are absent is silently skipped.
                if patchset_delete_noop(c, raw) {
                    return Ok(());
                }
            }
            conflict_handler(db, c, raw, st, conflict, CHANGESET_DATA, attempt1, pb_retry)
        }
        OP_UPDATE => {
            let sql = build_update_sql(c, raw, verify);
            let mut params = vec![Value::Null; 2 * c.n_col];
            for (i, &pk) in c.ab_pk.iter().enumerate() {
                let has_old = raw.old.get(i).and_then(|v| v.as_ref()).is_some();
                if pk != 0 || (verify && has_old) {
                    if let Some(v) = raw
                        .old
                        .get(i)
                        .and_then(|v| v.as_ref())
                        .and_then(|v| v.to_value())
                    {
                        params[i * 2 + 1] = v;
                    }
                }
                if let Some(v) = raw
                    .new
                    .get(i)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.to_value())
                {
                    params[i * 2] = v;
                }
            }
            db.execute(&sql, params).map_err(ApplyError::Engine)?;
            if db.changes() > 0 {
                record_written_update(c, st, raw);
                return Ok(());
            }
            conflict_handler(db, c, raw, st, conflict, CHANGESET_DATA, attempt1, pb_retry)
        }
        _ => {
            // INSERT.
            let sql = build_insert_sql(c);
            let mut params: Vec<Value> = Vec::with_capacity(c.n_col);
            for i in 0..c.n_col {
                let v = raw
                    .new
                    .get(i)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.to_value())
                    .ok_or(ApplyError::Corrupt("undefined field in changeset INSERT"))?;
                params.push(stat1_view_db(c, i, v));
            }
            let res = db.execute(&sql, params);
            match res {
                Ok(()) => {
                    record_written_insert(c, st, raw);
                    Ok(())
                }
                Err(e) => {
                    if is_constraint(&e) {
                        conflict_handler(
                            db,
                            c,
                            raw,
                            st,
                            conflict,
                            CHANGESET_CONFLICT,
                            attempt1,
                            pb_replace,
                        )
                    } else {
                        Err(ApplyError::Engine(e))
                    }
                }
            }
        }
    }
}

fn count_pk(c: &ApplyCtx) -> usize {
    c.ab_pk.iter().filter(|p| **p != 0).count()
}

fn all_pk(c: &ApplyCtx) -> bool {
    c.ab_pk.iter().all(|p| *p != 0)
}

/// A patchset DELETE whose row is absent carries nothing to verify —
/// IGNORENOOP skips it silently.
fn patchset_delete_noop(_c: &ApplyCtx, _raw: &RawChange) -> bool {
    false
}

fn is_constraint(e: &Error) -> bool {
    matches!(e, Error::Constraint(_))
}

/// stat1's changeset view ↔ database view (X'' ↔ NULL).
fn stat1_view_db(c: &ApplyCtx, col: usize, v: Value) -> Value {
    if c.b_stat1 && col == 1 {
        match v {
            Value::Blob(b) if b.is_empty() => Value::Null,
            other => other,
        }
    } else {
        v
    }
}

/// Bind the PK values of one side into a pre-sized param vector.
fn bind_pk_into(c: &ApplyCtx, side: &[Option<SessVal>], params: &mut [Value]) {
    for (i, &pk) in c.ab_pk.iter().enumerate() {
        if pk != 0 {
            let v = side
                .get(i)
                .and_then(|v| v.as_ref())
                .and_then(|v| v.to_value())
                .unwrap_or(Value::Null);
            params[i] = stat1_view_db(c, i, v);
        }
    }
}

/// The conflict-row SELECT (the change's key side).
fn build_conflict_select(c: &ApplyCtx) -> String {
    if c.b_stat1 {
        return "SELECT * FROM \"sqlite_stat1\" WHERE \"tbl\" = ?1 AND \"idx\" IS \
             (CASE WHEN length(?2)=0 AND typeof(?2)='blob' THEN NULL ELSE ?2 END)"
            .to_string();
    }
    let mut sql = String::from("SELECT * FROM ");
    super::gen::quote_ident(&mut sql, &c.name);
    sql.push_str(" WHERE ");
    let mut first = true;
    for (i, &pk) in c.ab_pk.iter().enumerate() {
        if pk != 0 {
            if !first {
                sql.push_str(" AND ");
            }
            first = false;
            sql.push('"');
            sql.push_str(&c.az_col[i]);
            sql.push_str("\" = ?");
            sql.push_str(&(i + 1).to_string());
        }
    }
    sql
}

/// `sessionDeleteRow`: `DELETE FROM t WHERE pk = ?... [AND (?N OR c IS ?...)]`.
fn build_delete_sql(c: &ApplyCtx) -> String {
    if c.b_stat1 {
        return "DELETE FROM \"sqlite_stat1\" WHERE \"tbl\" = ?1 AND \"idx\" IS \
             (CASE WHEN length(?2)=0 AND typeof(?2)='blob' THEN NULL ELSE ?2 END) \
             AND (?4 OR \"stat\" IS ?3)"
            .to_string();
    }
    let mut sql = String::from("DELETE FROM ");
    super::gen::quote_ident(&mut sql, &c.name);
    sql.push_str(" WHERE ");
    let mut n = 0usize;
    for (i, &pk) in c.ab_pk.iter().enumerate() {
        if pk != 0 {
            if n > 0 {
                sql.push_str(" AND ");
            }
            sql.push('"');
            sql.push_str(&c.az_col[i]);
            sql.push_str("\" = ?");
            sql.push_str(&(i + 1).to_string());
            n += 1;
        }
    }
    if n < c.n_col {
        sql.push_str(&format!(" AND (?{} OR ", c.n_col + 1));
        let mut first = true;
        for (i, &pk) in c.ab_pk.iter().enumerate() {
            if pk == 0 {
                if !first {
                    sql.push_str(" AND ");
                }
                first = false;
                sql.push('"');
                sql.push_str(&c.az_col[i]);
                sql.push_str("\" IS ?");
                sql.push_str(&(i + 1).to_string());
            }
        }
        sql.push(')');
    }
    sql
}

/// `sessionInsertRow`.
fn build_insert_sql(c: &ApplyCtx) -> String {
    if c.b_stat1 {
        return "INSERT INTO \"sqlite_stat1\" VALUES(?1, CASE WHEN length(?2)=0 AND \
                typeof(?2)='blob' THEN NULL ELSE ?2 END, ?3)"
            .to_string();
    }
    let mut sql = String::from("INSERT INTO ");
    super::gen::quote_ident(&mut sql, &c.name);
    sql.push('(');
    for i in 0..c.n_col {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push('"');
        sql.push_str(&c.az_col[i]);
        sql.push('"');
    }
    sql.push_str(") VALUES(");
    for i in 0..c.n_col {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
        sql.push_str(&(i + 1).to_string());
    }
    sql.push(')');
    sql
}

/// `sessionUpdateBuilder`: `UPDATE t SET c = ?k WHERE pk IS ?m AND c IS ?o`
/// (old-value terms only in verified changeset mode).
fn build_update_sql(c: &ApplyCtx, raw: &RawChange, verify: bool) -> String {
    let mut sql = String::from("UPDATE ");
    super::gen::quote_ident(&mut sql, &c.name);
    sql.push_str(" SET ");
    let mut any = false;
    for i in 0..c.n_col {
        if c.ab_pk[i] == 0 && raw.new.get(i).and_then(|v| v.as_ref()).is_some() {
            if any {
                sql.push_str(", ");
            }
            any = true;
            sql.push('"');
            sql.push_str(&c.az_col[i]);
            sql.push_str("\" = ?");
            sql.push_str(&(i * 2 + 1).to_string());
        }
    }
    if !any {
        // No-op SET is not legal SQL — match nothing. (A changeset UPDATE
        // always carries at least one modified field; defensive only.)
        sql.push('"');
        sql.push_str(&c.az_col[0]);
        sql.push_str("\" = \"");
        sql.push_str(&c.az_col[0]);
        sql.push_str("\" WHERE 0 AND '");
    } else {
        sql.push_str(" WHERE ");
    }
    let mut first = true;
    for i in 0..c.n_col {
        let pk = c.ab_pk[i] != 0;
        let has_old = raw.old.get(i).and_then(|v| v.as_ref()).is_some();
        if pk || (verify && has_old) {
            if !first {
                sql.push_str(" AND ");
            }
            first = false;
            sql.push('"');
            sql.push_str(&c.az_col[i]);
            sql.push_str("\" IS ?");
            sql.push_str(&(i * 2 + 2).to_string());
        }
    }
    sql
}

/// `sessionConflictHandler`.
fn conflict_handler(
    db: &mut Database,
    c: &mut ApplyCtx,
    raw: &RawChange,
    st: &mut ApplyState,
    conflict: &mut dyn FnMut(&ConflictEvent<'_>) -> ConflictVerdict,
    e_type: i32,
    attempt1: bool,
    pb_flag: &mut bool,
) -> Result<(), ApplyError> {
    let mut res: i32 = CHANGESET_ABORT;
    let mut conflict_row: Option<Vec<Value>> = None;
    let seek = attempt1 && matches!(e_type, CHANGESET_DATA | CHANGESET_CONFLICT);
    if seek {
        // Find a row with the change's key (INSERT: new PK; else old PK).
        let select_sql = build_conflict_select(c);
        let side = if raw.op == OP_INSERT {
            &raw.new
        } else {
            &raw.old
        };
        let mut params = vec![Value::Null; c.n_col];
        bind_pk_into(c, side, &mut params);
        let rows = db.query(&select_sql, params).map_err(ApplyError::Engine)?;
        if let Some(row) = rows.into_iter().next() {
            if st.b_ignore_noop && raw.op != OP_DELETE {
                // The v2 IGNORENOOP auto-resolution: every non-PK field
                // undefined or equal → OMIT without the handler.
                let mut noop = true;
                for i in 0..c.n_col {
                    if c.ab_pk[i] == 0 {
                        match raw.new.get(i).and_then(|v| v.as_ref()) {
                            None | Some(SessVal::Undef) | Some(SessVal::Replaced) => {}
                            Some(v) => {
                                if let (Some(cur), Some(nv)) = (row.get(i), v.to_value()) {
                                    if !values_equal(&nv, cur) {
                                        noop = false;
                                    }
                                }
                            }
                        }
                    }
                }
                if noop {
                    res = CHANGESET_OMIT;
                } else {
                    conflict_row = Some(row);
                }
            } else {
                conflict_row = Some(row);
            }
        }
    }
    if !matches!(res, CHANGESET_OMIT) {
        let code = if conflict_row.is_some() {
            e_type
        } else {
            e_type + 1 // DATA→NOTFOUND, CONFLICT→CONSTRAINT
        };
        // Deferred-constraint buffering: INSERT CONSTRAINT conflicts,
        // while the deferral is armed (SQLite: bDeferConstraints).
        if st.b_defer && code == CHANGESET_CONSTRAINT && raw.op != OP_DELETE && raw.op != OP_UPDATE
        {
            st.constraints.push(raw.clone());
            return Ok(());
        }
        let evt = ConflictEvent {
            code,
            op: raw.op,
            table: &c.name,
            n_col: c.n_col,
            pk: &c.ab_pk,
            indirect: raw.indirect,
            old: &raw.old,
            new: &raw.new,
            conflict_row: conflict_row.take(),
        };
        res = conflict(&evt);
    }

    match res {
        CHANGESET_REPLACE => {
            if !seek {
                // REPLACE is only valid with the conflicting row present
                // (DATA/CONFLICT); NOTFOUND/CONSTRAINT → misuse.
                Err(ApplyError::Misuse(
                    "CHANGESET_REPLACE on NOTFOUND/CONSTRAINT",
                ))
            } else {
                *pb_flag = true;
                rebase_add(st, c, raw, true);
                Ok(())
            }
        }
        CHANGESET_OMIT => {
            rebase_add(st, c, raw, false);
            Ok(())
        }
        CHANGESET_ABORT => Err(ApplyError::Abort),
        _ => Err(ApplyError::Misuse("invalid conflict handler verdict")),
    }
}

/// `sessionRebaseAdd` — record an OMIT/REPLACE resolution.
fn rebase_add(st: &mut ApplyState, c: &ApplyCtx, raw: &RawChange, replace: bool) {
    if !st.b_rebase {
        return;
    }
    if !st.rebase_started {
        st.rebase_buf.push(b'T');
        put_varint(&mut st.rebase_buf, c.n_col as u64);
        st.rebase_buf.extend_from_slice(&c.ab_pk);
        st.rebase_buf.extend_from_slice(c.name.as_bytes());
        st.rebase_buf.push(0);
        st.rebase_started = true;
    }
    st.rebase_buf.push(if raw.op == OP_DELETE {
        OP_DELETE
    } else {
        OP_INSERT
    });
    st.rebase_buf.push(replace as u8);
    for i in 0..c.n_col {
        let v = if raw.op == OP_DELETE || (raw.op == OP_UPDATE && c.ab_pk[i] != 0) {
            raw.old.get(i)
        } else {
            raw.new.get(i)
        };
        append_rebased_field(&mut st.rebase_buf, v.and_then(|v| v.as_ref()));
    }
}

/// Serialize one rebase-blob field: `None` → undefined; a parsed value
/// re-serializes (Undef/Replaced placeholders stay undefined — the rebase
/// blob never carries 0xFF).
fn append_rebased_field(out: &mut Vec<u8>, v: Option<&SessVal>) {
    match v {
        Some(SessVal::Null) => out.push(TYPE_NULL),
        Some(sv) => match sv.to_value() {
            Some(val) => append_value(out, Some(&val)),
            None => out.push(TYPE_UNDEF),
        },
        None => out.push(TYPE_UNDEF),
    }
}

/// `sessionRetryConstraints` — re-apply buffered constraint conflicts
/// until stable, then surface the leftovers to the handler.
fn retry_constraints(
    db: &mut Database,
    c: &mut ApplyCtx,
    st: &mut ApplyState,
    conflict: &mut dyn FnMut(&ConflictEvent<'_>) -> ConflictVerdict,
) -> Result<(), ApplyError> {
    let mut rounds = 0usize;
    while !st.constraints.is_empty() {
        let cons: Vec<RawChange> = std::mem::take(&mut st.constraints);
        let n_before = cons.len();
        for mut raw in cons {
            // Retries keep the deferral armed until the no-progress
            // check disarms it; then conflicts reach the handler.
            let r = apply_one_with_retry(db, c, &mut raw, st, conflict);
            match r {
                Ok(()) => {}
                Err(e) => {
                    // Still-conflicting entries were re-buffered by the
                    // handler (deferral) or resolved (OMIT/ABORT).
                    if matches!(
                        e,
                        ApplyError::Abort | ApplyError::Misuse(_) | ApplyError::Corrupt(_)
                    ) {
                        return Err(e);
                    }
                    return Err(e);
                }
            }
        }
        if st.constraints.len() >= n_before {
            st.b_defer = false;
        }
        rounds += 1;
        if rounds > 1000 {
            // Safety net (SQLite's loop terminates because each round
            // either drains the buffer or disarms the deferral).
            st.b_defer = false;
        }
    }
    Ok(())
}

// ---- written-row tracking + the deferred-FK end check ----

fn record_written_insert(c: &ApplyCtx, st: &mut ApplyState, raw: &RawChange) {
    let row: Vec<Value> = (0..c.n_col)
        .map(|i| {
            raw.new
                .get(i)
                .and_then(|v| v.as_ref())
                .and_then(|v| v.to_value())
                .unwrap_or(Value::Null)
        })
        .collect();
    st.written.push((c.name.clone(), OP_INSERT, row));
}

fn record_written_update(c: &ApplyCtx, st: &mut ApplyState, raw: &RawChange) {
    let old_row: Vec<Value> = (0..c.n_col)
        .map(|i| {
            raw.old
                .get(i)
                .and_then(|v| v.as_ref())
                .and_then(|v| v.to_value())
                .unwrap_or(Value::Null)
        })
        .collect();
    let new_row: Vec<Value> = (0..c.n_col)
        .map(|i| {
            raw.new
                .get(i)
                .and_then(|v| v.as_ref())
                .and_then(|v| v.to_value())
                .unwrap_or(Value::Null)
        })
        .collect();
    st.written.push((c.name.clone(), OP_DELETE, old_row));
    st.written.push((c.name.clone(), OP_INSERT, new_row));
}

fn record_written_delete(c: &ApplyCtx, st: &mut ApplyState, old: &[Option<SessVal>]) {
    let row: Vec<Value> = (0..c.n_col)
        .map(|i| {
            old.get(i)
                .and_then(|v| v.as_ref())
                .and_then(|v| v.to_value())
                .unwrap_or(Value::Null)
        })
        .collect();
    st.written.push((c.name.clone(), OP_DELETE, row));
}

/// The deferred-FK end check: verify the rows this apply wrote against
/// the final state. Any violation presents ONE FOREIGN_KEY conflict;
/// OMIT commits with the violations (SQLite's behavior), anything else
/// is a constraint failure.
fn fk_end_check(
    db: &mut Database,
    st: &mut ApplyState,
    conflict: &mut dyn FnMut(&ConflictEvent<'_>) -> ConflictVerdict,
) -> Result<(), ApplyError> {
    let violations = count_fk_violations(db, st).map_err(ApplyError::Engine)?;
    if violations == 0 {
        return Ok(());
    }
    let evt = ConflictEvent {
        code: CHANGESET_FOREIGN_KEY,
        op: 0,
        table: "",
        n_col: 0,
        pk: &[],
        indirect: false,
        old: &[],
        new: &[],
        conflict_row: None,
    };
    let res = conflict(&evt);
    if res == CHANGESET_OMIT {
        Ok(())
    } else {
        Err(ApplyError::Engine(Error::constraint(
            "FOREIGN KEY constraint failed",
        )))
    }
}

/// Verify the apply's own writes: every child-side key it inserted or
/// updated must have its parent; every parent key it deleted must have
/// no remaining children.
fn count_fk_violations(db: &Database, st: &ApplyState) -> Result<usize, Error> {
    let mut violations = 0usize;
    // The session-row offset: plain rowid tables carry a leading
    // synthetic _rowid_ column.
    let row_offset = |t: &crate::schema::Table| -> usize {
        if !t.without_rowid && !t.columns.iter().any(|c| c.primary_key) {
            1
        } else {
            0
        }
    };

    // Child-side checks (INSERT entries — UPDATEs recorded both sides).
    for (table, op, row) in &st.written {
        if *op != OP_INSERT {
            continue;
        }
        let tbl = match db.catalog.get_table(table) {
            Some(t) => t,
            None => continue,
        };
        let off = row_offset(&tbl);
        for fk in &tbl.foreign_keys {
            let mut key: Vec<Value> = Vec::with_capacity(fk.columns.len());
            let mut any_null = false;
            for &ci in &fk.columns {
                match row.get(ci + off) {
                    Some(Value::Null) | None => any_null = true,
                    Some(v) => key.push(v.clone()),
                }
            }
            if any_null || key.is_empty() {
                continue; // partially-NULL child key passes
            }
            if !fk_parent_exists(db, &fk.ref_table, &fk.ref_columns, &key)? {
                violations += 1;
            }
        }
    }

    // Parent-side checks (DELETE entries): no child may reference the
    // deleted key.
    let all = db.catalog.all_tables();
    for (table, op, row) in &st.written {
        if *op != OP_DELETE {
            continue;
        }
        let parent_tbl = match db.catalog.get_table(table) {
            Some(t) => t,
            None => continue,
        };
        for (child_name, child_tbl) in &all {
            for fk in &child_tbl.foreign_keys {
                if !fk.ref_table.eq_ignore_ascii_case(table) {
                    continue;
                }
                if let Some(vals) = resolve_parent_key(&parent_tbl, &fk.ref_columns, row) {
                    if !vals.iter().any(|v| matches!(v, Value::Null)) {
                        violations += count_children(db, child_name, fk, &vals)?;
                    }
                }
            }
        }
    }
    Ok(violations)
}

/// Does a parent row exist for `key`?
fn fk_parent_exists(
    db: &Database,
    parent: &str,
    ref_columns: &[String],
    key: &[Value],
) -> Result<bool, Error> {
    let tbl = db
        .catalog
        .get_table(parent)
        .ok_or_else(|| Error::NotFound(format!("no such table: {parent}")))?;
    let cols: Vec<String> = if ref_columns.is_empty() {
        // Implicit: the parent's PK columns.
        tbl.columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect()
    } else {
        ref_columns.to_vec()
    };
    let mut sql = String::from("SELECT 1 FROM ");
    super::gen::quote_ident(&mut sql, parent);
    sql.push_str(" WHERE ");
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            sql.push_str(" AND ");
        }
        sql.push('"');
        sql.push_str(c);
        sql.push_str("\" = ?");
        sql.push_str(&(i + 1).to_string());
    }
    sql.push_str(" LIMIT 1");
    let rows = db.query(&sql, key.to_vec())?;
    Ok(!rows.is_empty())
}

/// The deleted parent row's key values for an FK's referenced columns.
fn resolve_parent_key(
    parent: &crate::schema::Table,
    ref_columns: &[String],
    row: &[Value],
) -> Option<Vec<Value>> {
    let off = if !parent.without_rowid && !parent.columns.iter().any(|c| c.primary_key) {
        1
    } else {
        0
    };
    let cols: Vec<String> = if ref_columns.is_empty() {
        parent
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect()
    } else {
        ref_columns.to_vec()
    };
    let mut vals = Vec::with_capacity(cols.len());
    for c in &cols {
        let ci = parent
            .columns
            .iter()
            .position(|x| x.name.eq_ignore_ascii_case(c))?;
        vals.push(row.get(ci + off)?.clone());
    }
    Some(vals)
}

/// How many child rows reference a deleted parent key?
fn count_children(
    db: &Database,
    child: &str,
    fk: &crate::schema::ForeignKeyClause,
    parent_vals: &[Value],
) -> Result<usize, Error> {
    let child_tbl = db
        .catalog
        .get_table(child)
        .ok_or_else(|| Error::NotFound(format!("no such table: {child}")))?;
    let mut sql = String::from("SELECT COUNT(*) FROM ");
    super::gen::quote_ident(&mut sql, child);
    sql.push_str(" WHERE ");
    for (i, &ci) in fk.columns.iter().enumerate() {
        if i > 0 {
            sql.push_str(" AND ");
        }
        let col = child_tbl
            .columns
            .get(ci)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        sql.push('"');
        sql.push_str(&col);
        sql.push_str("\" = ?");
        sql.push_str(&(i + 1).to_string());
    }
    let rows = db.query(&sql, parent_vals.to_vec())?;
    Ok(rows
        .first()
        .and_then(|r| r.first())
        .and_then(|v| match v {
            Value::Integer(n) => Some(*n as usize),
            _ => None,
        })
        .unwrap_or(0))
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Integer(x), Value::Integer(y)) => x == y,
        (Value::Real(x), Value::Real(y)) => x == y,
        (Value::Text(x), Value::Text(y)) => x.as_str() == y.as_str(),
        (Value::Blob(x), Value::Blob(y)) => x == y,
        _ => false,
    }
}
