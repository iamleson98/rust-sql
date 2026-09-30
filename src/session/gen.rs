//! Changeset / patchset generation — SQLite's `sessionGenerateChangeset`
//! (`sqlite3session_changeset` / `_patchset`).
//!
//! The capture stores the FIRST change per row key; generation resolves
//! each entry against the CURRENT database state:
//!
//!   entry op   row in db   emitted
//!   ---------  ----------  --------------------------------------
//!   INSERT     yes         INSERT with the CURRENT row values
//!   INSERT     no          (nothing — insert then delete)
//!   UPDATE     yes         UPDATE old=captured, new=current (skipped
//!                          when nothing differs — the self-healing
//!                          no-op collapse, including DELETE+re-INSERT
//!                          with identical values)
//!   UPDATE     no          DELETE with the captured old record
//!   DELETE     yes         UPDATE old=captured, new=current (the
//!                          DELETE+INSERT → UPDATE coalesce)
//!   DELETE     no          DELETE with the captured old record
//!
//! A table that emits nothing under its header is REWOUND (the header
//! disappears — SQLite's `nRewind` discipline).

use super::codec::*;
use super::state::{SessionCore, SessionTable};
use crate::api::Database;
use crate::error::Error;
use crate::types::Value;

/// Generate a changeset (`patchset=false`) or patchset (`true`) from the
/// session's recorded changes. `db` supplies the current-state
/// resolution; the session's stored records are repaired/padded first if
/// the schema drifted since capture (SQLite revalidates at generation).
pub fn generate(core: &mut SessionCore, db: &Database, patchset: bool) -> Result<Vec<u8>, Error> {
    if let Some(e) = &core.rc {
        return Err(Error::Runtime(e.0.clone()));
    }
    let mut out: Vec<u8> = Vec::new();
    for ti in 0..core.tables.len() {
        if core.tables[ti].n_entry == 0 {
            continue;
        }
        let name = core.tables[ti].name.clone();
        let tbl = db
            .catalog
            .get_table(&name)
            .ok_or_else(|| Error::NotFound(format!("no such table: {name}")))?;
        {
            let implicit = core.implicit_pk;
            let model = &mut core.tables[ti];
            if !model.initialized {
                SessionCore::reinit_and_pad(model, &tbl, implicit).map_err(Error::Runtime)?;
            } else if model.untrackable {
                // Ignored table (SQLite's default for no-PK tables).
                continue;
            } else {
                let expect_n = expected_model_n(&tbl, &name);
                if model.n_col != expect_n {
                    SessionCore::reinit_and_pad(model, &tbl, implicit).map_err(Error::Runtime)?;
                }
            }
        }
        let t = &core.tables[ti];

        // The per-table buffer (rewound when nothing is emitted).
        let rewind = out.len();
        append_table_hdr(&mut out, patchset, t);

        // The resolution SELECT, built once per table; re-bound per entry
        // with the stored record's PK values.
        let select_sql = build_select(t);
        let mut emitted_any = false;

        for bucket in 0..t.buckets.len() {
            let ids: Vec<usize> = t.buckets[bucket].clone();
            for id in ids {
                let entry = match t.entries.get(id).and_then(|e| e.as_ref()) {
                    Some(e) => e,
                    None => continue,
                };
                let pk_vals = match pk_values_of(t, &entry.record) {
                    Some(v) => v,
                    None => continue,
                };
                let rows = db.query(&select_sql, pk_vals)?;
                let current = rows.into_iter().next();
                match (entry.op, current) {
                    (OP_INSERT, Some(row)) => {
                        out.push(OP_INSERT);
                        out.push(entry.indirect as u8);
                        if row.len() != t.n_col {
                            return Err(Error::Corruption("session resolution width".into()));
                        }
                        for v in &row {
                            append_value(&mut out, Some(v));
                        }
                        emitted_any = true;
                    }
                    (OP_INSERT, None) => {}
                    (_, Some(row)) => {
                        if row.len() != t.n_col {
                            return Err(Error::Corruption("session resolution width".into()));
                        }
                        if append_update(&mut out, t, patchset, entry, &row) {
                            emitted_any = true;
                        }
                    }
                    (_, None) => {
                        append_delete(&mut out, t, patchset, entry);
                        emitted_any = true;
                    }
                }
            }
        }
        if !emitted_any {
            out.truncate(rewind);
        }
    }
    Ok(out)
}

/// The session-model column count for a live engine table.
pub(crate) fn expected_model_n(tbl: &crate::schema::Table, name: &str) -> usize {
    let b_stat1 = name.eq_ignore_ascii_case("sqlite_stat1");
    let b_rowid = !tbl.without_rowid && !tbl.columns.iter().any(|c| c.primary_key) && !b_stat1;
    let _ = b_stat1;
    tbl.columns.len() + if b_rowid { 1 } else { 0 }
}

/// The table header: 'T'/'P', varint nCol, PK flags, NUL-terminated name.
pub(crate) fn append_table_hdr(out: &mut Vec<u8>, patchset: bool, t: &SessionTable) {
    out.push(if patchset { b'P' } else { b'T' });
    put_varint(out, t.n_col as u64);
    for &p in &t.ab_pk {
        out.push(p);
    }
    out.extend_from_slice(t.name.as_bytes());
    out.push(0);
}

/// The resolution SELECT by primary key (`sessionSelectStmt`).
fn build_select(t: &SessionTable) -> String {
    if t.b_stat1 {
        return "SELECT \"tbl\", CASE WHEN \"idx\" IS NULL THEN X'' ELSE \"idx\" END, \"stat\" \
             FROM \"sqlite_stat1\" WHERE \"tbl\" = ?1 AND \"idx\" IS \
             (CASE WHEN length(?2)=0 AND typeof(?2)='blob' THEN NULL ELSE ?2 END)"
            .to_string();
    }
    let mut sql = String::with_capacity(64 + 24 * t.n_col);
    if t.b_rowid {
        sql.push_str("SELECT \"_rowid_\", * FROM ");
        quote_ident(&mut sql, &t.name);
        sql.push_str(" WHERE \"_rowid_\" = ?1");
        return sql;
    }
    sql.push_str("SELECT * FROM ");
    quote_ident(&mut sql, &t.name);
    sql.push_str(" WHERE ");
    let mut first = true;
    for (i, &pk) in t.ab_pk.iter().enumerate() {
        if pk != 0 {
            if !first {
                sql.push_str(" AND ");
            }
            first = false;
            sql.push('"');
            sql.push_str(&t.az_col[i]);
            sql.push_str("\" = ?");
            sql.push_str(&(i + 1).to_string());
        }
    }
    sql
}

pub(crate) fn quote_ident(sql: &mut String, name: &str) {
    sql.push('"');
    for c in name.chars() {
        if c == '"' {
            sql.push('"');
        }
        sql.push(c);
    }
    sql.push('"');
}

/// The PK values serialized in a stored record, in PK order (used for
/// both the resolution SELECT's binds and the rebase/changegroup paths).
pub(crate) fn pk_values_of(t: &SessionTable, rec: &[u8]) -> Option<Vec<Value>> {
    let mut vals = Vec::with_capacity(4);
    let mut pos = 0usize;
    for &pk in t.ab_pk.iter() {
        let (v, l) = parse_value(rec, pos).ok()?;
        if pk != 0 {
            vals.push(v.to_value()?);
        }
        pos += l;
    }
    Some(vals)
}

/// `sessionAppendUpdate`: emit an UPDATE (or a patchset UPDATE record)
/// from the captured old record vs the current row. Returns false when
/// the change is a no-op (nothing appended). The old record's fields are
/// copied VERBATIM (byte-faithful), never re-serialized.
fn append_update(
    out: &mut Vec<u8>,
    t: &SessionTable,
    patchset: bool,
    entry: &super::state::SessionChange,
    current: &[Value],
) -> bool {
    // Field offsets in the stored record.
    let mut starts: Vec<(usize, usize)> = Vec::with_capacity(t.n_col);
    let mut pos = 0usize;
    for _ in 0..t.n_col {
        let l = serial_len(&entry.record, pos);
        starts.push((pos, l));
        pos += l;
    }
    let mut changed = vec![false; t.n_col];
    let mut any_changed = false;
    for i in 0..t.n_col {
        let (s, l) = starts[i];
        let differs = match parse_value(&entry.record, s) {
            Ok((v, _)) => {
                if v == SessVal::Undef || v == SessVal::Replaced {
                    true // captured-undefined always counts as changed
                } else {
                    match v.to_value() {
                        Some(old) => !values_equal(&old, &current[i]),
                        None => true,
                    }
                }
            }
            Err(_) => true,
        };
        let _ = l;
        if differs {
            changed[i] = true;
            any_changed = true;
        }
    }
    if !any_changed {
        return false;
    }
    out.push(OP_UPDATE);
    out.push(entry.indirect as u8);
    // old.* (changesets only): PK + changed fields carry the stored
    // bytes verbatim; the rest are undefined.
    if !patchset {
        for i in 0..t.n_col {
            if t.ab_pk[i] != 0 || changed[i] {
                let (s, l) = starts[i];
                out.extend_from_slice(&entry.record[s..s + l]);
            } else {
                out.push(TYPE_UNDEF);
            }
        }
    }
    // new.*: changed fields carry the current values (PK fields stay
    // undefined in changesets; patchsets also carry PK fields).
    for i in 0..t.n_col {
        if changed[i] || (patchset && t.ab_pk[i] != 0) {
            append_value(out, Some(&current[i]));
        } else {
            out.push(TYPE_UNDEF);
        }
    }
    true
}

/// `sessionAppendDelete`: DELETE with the full old record (changeset)
/// or the PK-only record (patchset).
fn append_delete(
    out: &mut Vec<u8>,
    t: &SessionTable,
    patchset: bool,
    entry: &super::state::SessionChange,
) {
    out.push(OP_DELETE);
    out.push(entry.indirect as u8);
    if !patchset {
        out.extend_from_slice(&entry.record);
    } else {
        let mut pos = 0usize;
        for &pk in t.ab_pk.iter() {
            let l = serial_len(&entry.record, pos);
            if pk != 0 {
                out.extend_from_slice(&entry.record[pos..pos + l]);
            }
            pos += l;
        }
    }
}

/// `sessionAppendUpdate`'s bChanged comparison: same type + same value
/// (SQLite compares via the column accessors; NULL==NULL counts as
/// unchanged).
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
