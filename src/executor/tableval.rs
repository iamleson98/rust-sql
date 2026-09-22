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
        "pragma_index_info" => pragma_index_info(ctx, &vals, false),
        "pragma_index_xinfo" => pragma_index_info(ctx, &vals, true),
        "pragma_foreign_key_list" => pragma_foreign_key_list(ctx, &vals)?,
        "pragma_collation_list" => pragma_collation_list(),
        "pragma_database_list" => pragma_database_list(),
        "dbstat" => dbstat(ctx, &vals)?,
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
//
// SQLite semantics for ALL of them: a missing table/index (or a non-text
// argument — an unbound parameter reads as NULL) yields ZERO rows, not
// an error — schema-discovery tooling (sea-schema, sea-orm-cli,
// sqlite_master crawlers) relies on the empty result, and the compat
// layer's prepare-time column discovery executes SELECTs once with
// UNBOUND parameters, where an error would zero out the column layout.
// ---------------------------------------------------------------------------

fn pragma_table_info(
    ctx: &ExecContext<'_>,
    args: &[Value],
) -> Result<(Vec<&'static str>, Vec<Vec<Value>>), Error> {
    const COLS: [&str; 6] = ["cid", "name", "type", "notnull", "dflt_value", "pk"];
    let name = match args.first() {
        Some(Value::Text(t)) => t.to_string(),
        _ => return Ok((COLS.to_vec(), Vec::new())),
    };
    let Some(table) = ctx.catalog().get_table(&name) else {
        return Ok((COLS.to_vec(), Vec::new()));
    };
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
    let Some(t) = ctx.catalog().get_table(&name) else {
        return (COLS.to_vec(), Vec::new());
    };
    // Delegate to the PRAGMA-path implementation: origin 'pk'/'u'/'c',
    // partial flag, and SQLite's reverse-creation-order seq — the TVF
    // must report metadata identical to `PRAGMA index_list` (a stale
    // divergent copy here once reported origin 'c' for PK autoindexes).
    let pr = crate::api::pragma_index_list(&t, ctx.catalog());
    (COLS.to_vec(), pr.rows)
}

fn pragma_index_info(
    ctx: &ExecContext<'_>,
    args: &[Value],
    xinfo: bool,
) -> (Vec<&'static str>, Vec<Vec<Value>>) {
    const COLS: [&str; 3] = ["seqno", "cid", "name"];
    const XCOLS: [&str; 6] = ["seqno", "cid", "name", "desc", "coll", "key"];
    let name = match args.first() {
        Some(Value::Text(t)) => t.to_string(),
        _ => {
            return if xinfo {
                (XCOLS.to_vec(), Vec::new())
            } else {
                (COLS.to_vec(), Vec::new())
            }
        }
    };
    let Some(idx) = ctx.catalog().get_index(&name) else {
        return if xinfo {
            (XCOLS.to_vec(), Vec::new())
        } else {
            (COLS.to_vec(), Vec::new())
        };
    };
    let Some(table) = ctx.catalog().get_table(&idx.table) else {
        return if xinfo {
            (XCOLS.to_vec(), Vec::new())
        } else {
            (COLS.to_vec(), Vec::new())
        };
    };
    // Delegate to the PRAGMA-path implementation (xinfo carries the
    // desc/coll/key columns and the auxiliary rowid entry).
    let pr = crate::api::pragma_index_info(&idx, &table, xinfo);
    if xinfo {
        (XCOLS.to_vec(), pr.rows)
    } else {
        (COLS.to_vec(), pr.rows)
    }
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
    let name = match args.first() {
        Some(Value::Text(t)) => t.to_string(),
        _ => return Ok((COLS.to_vec(), Vec::new())),
    };
    let Some(table) = ctx.catalog().get_table(&name) else {
        return Ok((COLS.to_vec(), Vec::new()));
    };
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

// ---------------------------------------------------------------------------
// dbstat — SQLite's DBSTAT_VTAB (per-page b-tree statistics)
// ---------------------------------------------------------------------------
//
// `FROM dbstat` (eponymous — no CREATE VIRTUAL TABLE, resolved by the
// planner when the catalog misses) or `FROM dbstat('t1')` (one object's
// pages). Second argument nonzero/'aggregate' = AGGREGATE mode (one row
// per b-tree with the sums). Columns are SQLite's: name, path, pageno,
// pagetype, ncell, payload, unused, mx_payload, pgoffset, pgsize.
//
// PATH format (engine's documented shape — SQLite's is opaque and
// explicitly subject to change): root = "/"; the i-th child of an
// interior page = parent + "/" + i (1-based, the rightmost pointer is
// the last child); an overflow chain page = its leaf path + "/" + the
// overflow page number.
//
// payload: payload bytes STORED ON the page (the local prefix for
// spilled cells, the chunk bytes for overflow pages). mx_payload: the
// largest TOTAL payload among cells starting on the page. unused: the
// page's free space. Freelist pages are not reported (as in SQLite).

/// dbstat's column set (also mirrored in namecheck's `tvf_columns` and
/// the eponymous `table_source` fallback — keep the three in sync).
pub(crate) const DBSTAT_COLS: [&str; 10] = [
    "name",
    "path",
    "pageno",
    "pagetype",
    "ncell",
    "payload",
    "unused",
    "mx_payload",
    "pgoffset",
    "pgsize",
];

/// One b-tree's page inventory, accumulated by the walk (aggregate
/// mode's sums; per-page mode keeps the rows).
struct DbstatAccum {
    ncell: i64,
    payload: i64,
    unused: i64,
    mx_payload: i64,
    pages: i64,
}

fn dbstat(
    ctx: &ExecContext<'_>,
    args: &[Value],
) -> Result<(Vec<&'static str>, Vec<Vec<Value>>), Error> {
    // Optional first argument: restrict to one table/index by name
    // (empty string or NULL = everything). Optional second: aggregate
    // mode flag (any nonzero integer, or a nonempty string).
    let filter: Option<String> = match args.first() {
        Some(Value::Text(t)) if !t.is_empty() => Some(t.to_string()),
        _ => None,
    };
    let aggregate = match args.get(1) {
        Some(Value::Integer(n)) => *n != 0,
        Some(Value::Text(t)) => !t.is_empty(),
        _ => false,
    };

    let pager = ctx.pager;
    let psz = pager.page_size() as usize;

    // B-tree inventory: the schema b-tree (root 0) plus every table and
    // index with its LIVE root (the ctx's root resolution includes the
    // session's bookkeeping overrides).
    let mut btrees: Vec<(String, u32, bool)> = Vec::new();
    btrees.push(("sqlite_master".to_string(), 0, false));
    let cat = ctx.catalog();
    for (name, t) in cat.all_tables() {
        if t.vtab.is_some() {
            continue; // virtual tables have no b-tree pages
        }
        let root = ctx.table_root(&t);
        if root == 0 {
            continue;
        }
        btrees.push((name, root, false));
    }
    for (name, ix) in cat.all_indexes() {
        let root = ctx.index_root(&ix);
        if root == 0 {
            continue;
        }
        btrees.push((name, root, true));
    }

    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut visited = std::collections::HashSet::new();
    for (name, root, is_index) in btrees {
        if let Some(f) = &filter {
            if !name.eq_ignore_ascii_case(f) {
                continue;
            }
        }
        visited.clear();
        let mut accum = DbstatAccum {
            ncell: 0,
            payload: 0,
            unused: 0,
            mx_payload: 0,
            pages: 0,
        };
        walk_dbstat_btree(
            pager,
            root,
            is_index,
            psz,
            &name,
            "/",
            aggregate,
            &mut rows,
            &mut accum,
            &mut visited,
        )?;
        if aggregate {
            rows.push(vec![
                Value::Text(name.as_str().into()),
                Value::Null,
                Value::Integer(root as i64),
                Value::Null,
                Value::Integer(accum.ncell),
                Value::Integer(accum.payload),
                Value::Integer(accum.unused),
                Value::Integer(accum.mx_payload),
                Value::Integer(0),
                Value::Integer(psz as i64),
            ]);
        }
    }
    Ok((DBSTAT_COLS.to_vec(), rows))
}

/// DFS over one b-tree's pages (and overflow chains). `visited` guards
/// against cycles in corrupt files — the bound is the file's page count.
fn walk_dbstat_btree(
    pager: &crate::storage::pager::Pager,
    pageno: u32,
    is_index: bool,
    psz: usize,
    name: &str,
    path: &str,
    aggregate: bool,
    rows: &mut Vec<Vec<Value>>,
    accum: &mut DbstatAccum,
    visited: &mut std::collections::HashSet<u32>,
) -> Result<(), Error> {
    use crate::storage::btree::Cell;
    use crate::storage::page::PageType;
    let _ = is_index; // page types drive the walk; the flag is informational

    // NOTE: no pageno==0 guard — page 0 is a REAL page here (the
    // schema b-tree's root; pages are 0-based in this engine). A 0
    // child/overflow pointer means "none" and is filtered at the call
    // sites.
    if !visited.insert(pageno) {
        return Err(Error::corruption(format!(
            "dbstat: page {pageno} visited twice (cycle?)"
        )));
    }
    if visited.len() > pager.n_pages() as usize + 8 {
        return Err(Error::corruption("dbstat: page walk exceeded the file"));
    }

    let page_ref = pager.get_page(pageno)?;
    let p = page_ref.lock();
    let pt = p.page_type()?;
    let ncell = p.n_cells() as usize;
    let unused = p.free_space() as i64;
    let mut payload_on_page: i64 = 0;
    let mut mx_payload: i64 = 0;
    let mut overflow_heads: Vec<(u32, u64)> = Vec::new();
    let mut children: Vec<u32> = Vec::new();
    for i in 0..ncell {
        let buf = p.cell_slice(i as u16)?;
        let cell = Cell::decode(buf, pt, psz as u32)?;
        match cell {
            Cell::TableLeaf { payload, .. } => {
                payload_on_page += payload.len() as i64;
                mx_payload = mx_payload.max(payload.len() as i64);
            }
            Cell::TableLeafOverflow {
                total,
                local,
                overflow,
                ..
            }
            | Cell::IndexLeafOverflow {
                total,
                local,
                overflow,
                ..
            } => {
                payload_on_page += local.len() as i64;
                mx_payload = mx_payload.max(total as i64);
                overflow_heads.push((overflow, total - local.len() as u64));
            }
            Cell::IndexInteriorOverflow {
                left_child,
                total,
                local,
                overflow,
                ..
            } => {
                payload_on_page += local.len() as i64;
                mx_payload = mx_payload.max(total as i64);
                overflow_heads.push((overflow, total - local.len() as u64));
                children.push(left_child);
            }
            Cell::IndexLeaf { key, .. } => {
                payload_on_page += key.len() as i64;
                mx_payload = mx_payload.max(key.len() as i64);
            }
            Cell::IndexInterior {
                key, left_child, ..
            } => {
                payload_on_page += key.len() as i64;
                mx_payload = mx_payload.max(key.len() as i64);
                children.push(left_child);
            }
            Cell::TableInterior { left_child, .. } => {
                children.push(left_child);
            }
        }
    }
    if let PageType::InteriorIndex | PageType::InteriorTable = pt {
        let right = p.right_most_pointer();
        if right != 0 {
            children.push(right);
        }
    }
    let pagetype = match pt {
        PageType::InteriorIndex | PageType::InteriorTable => "internal",
        PageType::LeafIndex | PageType::LeafTable => "leaf",
        PageType::Overflow => "overflow",
    };
    drop(p);

    accum.ncell += ncell as i64;
    accum.payload += payload_on_page;
    accum.unused += unused;
    accum.mx_payload = accum.mx_payload.max(mx_payload);
    accum.pages += 1;
    if !aggregate {
        rows.push(vec![
            Value::Text(name.into()),
            Value::Text(path.into()),
            Value::Integer(pageno as i64),
            Value::Text(pagetype.into()),
            Value::Integer(ncell as i64),
            Value::Integer(payload_on_page),
            Value::Integer(unused),
            Value::Integer(mx_payload),
            Value::Integer(pageno as i64 * psz as i64),
            Value::Integer(psz as i64),
        ]);
    }

    // Children: cell i of an interior page is child number i+1; the
    // rightmost pointer is the last child.
    for (i, child) in children.iter().enumerate() {
        if *child == 0 {
            continue;
        }
        let child_path = format!("{}/{}", path, i + 1);
        walk_dbstat_btree(
            pager,
            *child,
            is_index,
            psz,
            name,
            &child_path,
            aggregate,
            rows,
            accum,
            visited,
        )?;
    }
    // Overflow chains: one row per page, chunk-sized payload.
    for (head, tail_len) in overflow_heads {
        let mut cur = head;
        let mut remaining = tail_len as i64;
        let cap = (psz.saturating_sub(16)) as i64;
        let mut hops = 0usize;
        while cur != 0 && remaining > 0 {
            hops += 1;
            if hops > pager.n_pages() as usize + 8 {
                return Err(Error::corruption("dbstat: overflow chain cycle"));
            }
            if !visited.insert(cur) {
                return Err(Error::corruption(format!(
                    "dbstat: overflow page {cur} visited twice (cycle?)"
                )));
            }
            let take = remaining.min(cap);
            let page_unused = cap - take;
            let page_ref = pager.get_page(cur)?;
            let next = {
                let pg = page_ref.lock();
                let is_overflow = pg.page_type()? == PageType::Overflow;
                if !is_overflow {
                    return Err(Error::corruption(format!(
                        "dbstat: overflow chain hit non-overflow page {cur}"
                    )));
                }
                pg.overflow_next()
            };
            accum.payload += take;
            accum.unused += page_unused;
            accum.pages += 1;
            if !aggregate {
                rows.push(vec![
                    Value::Text(name.into()),
                    Value::Text(format!("{}/{}", path, cur).into()),
                    Value::Integer(cur as i64),
                    Value::Text("overflow".into()),
                    Value::Integer(0),
                    Value::Integer(take),
                    Value::Integer(page_unused),
                    Value::Integer(take),
                    Value::Integer(cur as i64 * psz as i64),
                    Value::Integer(psz as i64),
                ]);
            }
            remaining -= take;
            cur = next;
        }
    }
    Ok(())
}
