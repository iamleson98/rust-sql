//! Table-valued functions (FROM-clause function calls): `json_each`,
//! `json_tree`, and the `pragma_*` introspection family.
//!
//! SQLite exposes these as virtual tables whose arguments are evaluated
//! at execution time (bound parameters work, and each execution of the
//! statement re-evaluates them). We mirror the exact output schemas:
//!
//! * `json_each(x)` / `json_tree(x)` → `key, value, type, atom, id,
//!   parent, fullkey, path` (8 columns; `json_each` walks ONE level,
//!   `json_tree` walks the document recursively).
//! * `pragma_table_info('t')` → `cid, name, type, notnull, dflt_value, pk`
//! * `pragma_index_list('t')` → `seq, name, unique, origin, partial`
//! * `pragma_index_info('i')` → `seqno, cid, name`
//! * `pragma_foreign_key_list('t')` → `id, seq, table, from, to,
//!   on_update, on_delete, match`
//! * `pragma_collation_list()` → `seq, name`
//! * `pragma_database_list()` → `seq, name, file`

use crate::error::Error;
use crate::executor::{eval_row, ExecContext, ExecResult};
use crate::sql::ast::Expr;
use crate::types::Value;
use std::sync::Arc;

/// Evaluate a `Plan::TableFunction`: dispatch on the function name,
/// evaluate the argument expressions against the statement's bound
/// parameters, and materialize the result rows.
pub(crate) fn exec_table_function(
    ctx: &mut ExecContext<'_>,
    name: &str,
    args: &[Expr],
    alias: Option<&String>,
) -> Result<ExecResult, Error> {
    // Argument evaluation: constants and bound parameters resolve now.
    // Column references cannot (there is no outer row at FROM time) — a
    // lateral reference degrades to NULL, matching the v1 limitation.
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let mut vals: Vec<Value> = Vec::with_capacity(args.len());
    for a in args {
        vals.push(eval_row(
            a,
            &empty_row,
            &empty_cols,
            &ctx.params,
            &ctx.named_params,
        )?);
    }
    let fname = name.to_ascii_lowercase();
    let (bare_cols, rows): (Vec<&str>, Vec<Vec<Value>>) = match fname.as_str() {
        "json_each" => json_each(&vals, false)?,
        "json_tree" => json_each(&vals, true)?,
        "pragma_table_info" => pragma_table_info(ctx, &vals)?,
        "pragma_index_list" => pragma_index_list(ctx, &vals),
        "pragma_index_info" | "pragma_index_xinfo" => pragma_index_info(ctx, &vals),
        "pragma_foreign_key_list" => pragma_foreign_key_list(ctx, &vals)?,
        "pragma_collation_list" => pragma_collation_list(),
        "pragma_database_list" => pragma_database_list(),
        other => {
            return Err(Error::semantic(format!(
                "no such table-valued function: {}",
                other
            )))
        }
    };
    // Column names: "prefix.col" so qualified references resolve; the
    // evaluator's suffix fallback serves bare references (same contract
    // as table scans).
    let prefix = alias.cloned().unwrap_or_else(|| name.to_string());
    let columns: Arc<[String]> = bare_cols
        .iter()
        .map(|c| format!("{}.{}", prefix, c))
        .collect::<Vec<String>>()
        .into();
    Ok(ExecResult { columns, rows })
}

// ---------------------------------------------------------------------------
// JSON walkers (json_each / json_tree over the raw-preserving JSONB
// pipeline: `id`/`parent` are byte offsets in the canonical JSONB
// encoding — identical to SQLite's)
// ---------------------------------------------------------------------------

/// `json_each(x[, root])` (one level) / `json_tree(x[, root])`
/// (recursive) over the FIRST argument.
fn json_each(
    args: &[Value],
    recursive: bool,
) -> Result<(Vec<&'static str>, Vec<Vec<Value>>), Error> {
    const COLS: [&str; 8] = [
        "key", "value", "type", "atom", "id", "parent", "fullkey", "path",
    ];
    // Load the document: TEXT input is encoded to canonical JSONB (SQLite
    // walks the JSONB form, which is what makes the id/parent offsets line
    // up); a BLOB is walked AS GIVEN when it is valid JSONB (its own byte
    // offsets), with a text-parse fallback.
    let (bytes, sp): (Vec<u8>, crate::executor::jsonb::Sp) = match args.first() {
        None | Some(Value::Null) => return Ok((COLS.to_vec(), Vec::new())),
        Some(Value::Text(t)) => {
            let s: &str = t;
            match crate::executor::jsonb::parse_lenient(s) {
                Ok(jb) => {
                    let bytes = crate::executor::jsonb::jb_to_bytes(&jb);
                    let sp = crate::executor::jsonb::sp_from_bytes(&bytes)
                        .map_err(|_| Error::Runtime("malformed JSON".to_string()))?;
                    (bytes, sp)
                }
                Err(_) => {
                    return Err(Error::Runtime(format!(
                        "malformed JSON: {}",
                        truncate_for_err(s)
                    )))
                }
            }
        }
        Some(Value::Blob(b)) => match crate::executor::jsonb::sp_from_bytes(b) {
            Ok(sp) => (b.clone(), sp),
            Err(()) => {
                if let Ok(s) = std::str::from_utf8(b) {
                    if let Ok(jb) = crate::executor::jsonb::parse_lenient(s) {
                        let bytes = crate::executor::jsonb::jb_to_bytes(&jb);
                        let sp = crate::executor::jsonb::sp_from_bytes(&bytes)
                            .map_err(|_| Error::Runtime("malformed JSON".to_string()))?;
                        let segs = segs_of(args)?;
                        return Ok(walk_rows(&sp, &segs, recursive, COLS));
                    }
                }
                return Err(Error::Runtime("malformed JSON".to_string()));
            }
        },
        Some(other) => {
            let jb = if matches!(other, Value::Integer(_) | Value::Real(_)) {
                crate::executor::jsonb::value_to_jb(other)
            } else {
                match crate::executor::jsonb::parse_lenient(&other.as_text()) {
                    Ok(jb) => jb,
                    Err(_) => {
                        return Err(Error::Runtime(format!(
                            "malformed JSON: {}",
                            truncate_for_err(&other.as_text())
                        )))
                    }
                }
            };
            let bytes = crate::executor::jsonb::jb_to_bytes(&jb);
            let sp = crate::executor::jsonb::sp_from_bytes(&bytes)
                .map_err(|_| Error::Runtime("malformed JSON".to_string()))?;
            (bytes, sp)
        }
    };
    let _ = bytes;
    let segs = segs_of(args)?;
    Ok(walk_rows(&sp, &segs, recursive, COLS))
}

/// Path segments of the optional second argument (empty when absent);
/// a malformed path raises like SQLite's `bad JSON path`.
fn segs_of(args: &[Value]) -> Result<Vec<crate::executor::json::PathSeg>, Error> {
    match args.get(1) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(p) => {
            let s = p.as_text();
            match crate::executor::json::parse_path(&s) {
                Some(path) => Ok(path.segs().to_vec()),
                None => Err(Error::Runtime(format!("bad JSON path: '{}'", s))),
            }
        }
    }
}

/// Resolve the start node and emit the 8-column rows.
fn walk_rows(
    sp: &crate::executor::jsonb::Sp,
    segs: &[crate::executor::json::PathSeg],
    recursive: bool,
    cols: [&'static str; 8],
) -> (Vec<&'static str>, Vec<Vec<Value>>) {
    let Some(resolved) = crate::executor::jsonb::sp_resolve(sp, segs) else {
        // Path misses: no rows (SQLite: json_each('{"a":1}', '$.b') → empty).
        return (cols.to_vec(), Vec::new());
    };
    let rows = crate::executor::jsonb::each_rows(&resolved, recursive)
        .into_iter()
        .map(|r| {
            vec![
                r.key,
                r.value,
                Value::Text(r.type_name.into()),
                r.atom,
                Value::Integer(r.id),
                r.parent.map(Value::Integer).unwrap_or(Value::Null),
                Value::Text(r.fullkey.into()),
                Value::Text(r.path.into()),
            ]
        })
        .collect();
    (cols.to_vec(), rows)
}

fn truncate_for_err(s: &str) -> String {
    if s.len() > 48 {
        format!("{}...", &s[..48])
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// pragma_* introspection functions
// ---------------------------------------------------------------------------

fn table_arg(args: &[Value]) -> Result<String, Error> {
    match args.first() {
        Some(Value::Text(t)) => Ok(t.to_string()),
        _ => Err(Error::semantic(
            "pragma table-valued function requires a table-name text argument",
        )),
    }
}

fn pragma_table_info(
    ctx: &ExecContext<'_>,
    args: &[Value],
) -> Result<(Vec<&'static str>, Vec<Vec<Value>>), Error> {
    const COLS: [&str; 6] = ["cid", "name", "type", "notnull", "dflt_value", "pk"];
    let name = table_arg(args)?;
    let table = ctx
        .catalog()
        .get_table(&name)
        .ok_or_else(|| Error::semantic(format!("no such table: {}", name)))?;
    let mut rows = Vec::with_capacity(table.columns.len());
    for (i, c) in table.columns.iter().enumerate() {
        let empty_row: Vec<Value> = Vec::new();
        let empty_cols: Vec<String> = Vec::new();
        let dflt = c
            .default
            .as_ref()
            .map(|e| {
                eval_row(e, &empty_row, &empty_cols, &[], &Default::default())
                    .unwrap_or(Value::Null)
            })
            .unwrap_or(Value::Null);
        rows.push(vec![
            Value::Integer(i as i64),
            Value::Text(c.name.as_str().into()),
            Value::Text(c.declared_type.as_str().into()),
            Value::Integer(if c.explicit_not_null { 1 } else { 0 }),
            dflt,
            Value::Integer(c.pk_seq as i64),
        ]);
    }
    Ok((COLS.to_vec(), rows))
}

fn pragma_index_list(
    ctx: &ExecContext<'_>,
    args: &[Value],
) -> (Vec<&'static str>, Vec<Vec<Value>>) {
    const COLS: [&str; 5] = ["seq", "name", "unique", "origin", "partial"];
    let name = match args.first() {
        Some(Value::Text(t)) => t.to_string(),
        _ => return (COLS.to_vec(), Vec::new()),
    };
    let idxs = ctx.catalog().indexes_on_table(&name);
    let mut rows = Vec::with_capacity(idxs.len());
    for (i, ix) in idxs.iter().enumerate() {
        rows.push(vec![
            Value::Integer(i as i64),
            Value::Text(ix.name.as_str().into()),
            Value::Integer(if ix.unique { 1 } else { 0 }),
            Value::Text("c".into()),
            Value::Integer(0),
        ]);
    }
    (COLS.to_vec(), rows)
}

fn pragma_index_info(
    ctx: &ExecContext<'_>,
    args: &[Value],
) -> (Vec<&'static str>, Vec<Vec<Value>>) {
    const COLS: [&str; 3] = ["seqno", "cid", "name"];
    let name = match args.first() {
        Some(Value::Text(t)) => t.to_string(),
        _ => return (COLS.to_vec(), Vec::new()),
    };
    let idx = match ctx.catalog().get_index(&name) {
        Some(i) => i,
        None => return (COLS.to_vec(), Vec::new()),
    };
    let table = match ctx.catalog().get_table(&idx.table) {
        Some(t) => t,
        None => return (COLS.to_vec(), Vec::new()),
    };
    let mut rows = Vec::with_capacity(idx.columns.len());
    for (i, c) in idx.columns.iter().enumerate() {
        let cid = table.find_column(&c.name).map(|i| i as i64).unwrap_or(-1);
        rows.push(vec![
            Value::Integer(i as i64),
            Value::Integer(cid),
            Value::Text(c.name.as_str().into()),
        ]);
    }
    (COLS.to_vec(), rows)
}

fn pragma_foreign_key_list(
    ctx: &ExecContext<'_>,
    args: &[Value],
) -> Result<(Vec<&'static str>, Vec<Vec<Value>>), Error> {
    const COLS: [&str; 8] = [
        "id",
        "seq",
        "table",
        "from",
        "to",
        "on_update",
        "on_delete",
        "match",
    ];
    let name = table_arg(args)?;
    let table = ctx
        .catalog()
        .get_table(&name)
        .ok_or_else(|| Error::semantic(format!("no such table: {}", name)))?;
    let mut rows = Vec::new();
    for (id, fk) in table.foreign_keys.iter().enumerate() {
        for (seq, (from_idx, to_col)) in fk.columns.iter().zip(fk.ref_columns.iter()).enumerate() {
            let from_name = table
                .columns
                .get(*from_idx)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            rows.push(vec![
                Value::Integer(id as i64),
                Value::Integer(seq as i64),
                Value::Text(fk.ref_table.as_str().into()),
                Value::Text(from_name.as_str().into()),
                Value::Text(to_col.as_str().into()),
                Value::Text(action_sql(&fk.on_update).into()),
                Value::Text(action_sql(&fk.on_delete).into()),
                Value::Text("NONE".into()),
            ]);
        }
    }
    Ok((COLS.to_vec(), rows))
}

/// SQLite's PRAGMA foreign_key_list action spellings.
fn action_sql(a: &crate::sql::ast::ForeignKeyAction) -> &'static str {
    use crate::sql::ast::ForeignKeyAction::*;
    match a {
        NoAction => "NO ACTION",
        Restrict => "RESTRICT",
        SetNull => "SET NULL",
        SetDefault => "SET DEFAULT",
        Cascade => "CASCADE",
    }
}

fn pragma_collation_list() -> (Vec<&'static str>, Vec<Vec<Value>>) {
    const COLS: [&str; 2] = ["seq", "name"];
    let names = ["BINARY", "NOCASE", "RTRIM"];
    let rows = names
        .iter()
        .enumerate()
        .map(|(i, n)| vec![Value::Integer(i as i64), Value::Text((*n).into())])
        .collect();
    (COLS.to_vec(), rows)
}

fn pragma_database_list() -> (Vec<&'static str>, Vec<Vec<Value>>) {
    const COLS: [&str; 3] = ["seq", "name", "file"];
    (
        COLS.to_vec(),
        vec![vec![
            Value::Integer(0),
            Value::Text("main".into()),
            Value::Null,
        ]],
    )
}
