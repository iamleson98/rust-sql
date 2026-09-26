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
        "sqlite_dbdata" => dbdata(ctx, &vals)?,
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

// ---------------------------------------------------------------------------
// sqlite_dbdata — the SQLITE_DBDATA forensic page reader
// ---------------------------------------------------------------------------
//
// `FROM sqlite_dbdata('main')`: one row per (page, cell, field) of the
// RAW page image — no b-tree linkage followed, every page 0..n_pages
// visited in order (the forensic posture: unlinked pages, freelist
// pages and orphaned overflow chains are all visible). Columns are
// SQLite's: pgno, cell, field, value, hexval, descr.
//
// Engine-shape notes (documented divergences from SQLite's dbdata):
// * Pages are 0-BASED here (SQLite's are 1-based); page 0's cells begin
//   after the 100-byte database header.
// * The row codec is per-value self-describing (a leading tag byte per
//   value — see `encode_into`), not SQLite's record header; `field`
//   indexes the cell payload's values in stored order. The rowid-alias
//   marker (tag 0x09) is reported as its own field with type
//   "rowid-marker".
// * Index cells store ORDER-KEY encoded columns (tags 0x00..0x03 + BE
//   u32 lengths / total-order double keys); fields report those bytes.
// * `field = -1` is the cell's KEY: the rowid of a table/index leaf
//   cell, or the full (child, separator) bytes of an interior cell.
// * `cell = -1` is a page-level fact: the 12-byte page header, an
//   overflow page's chunk, a freelist trunk's header+entries, or a
//   zeroed/unknown page.
// * This engine ZEROES pages when it frees them (cache hygiene), so
//   freed pages decode as "zeroed (freed)" — deleted-row recovery via
//   freelist scanning is NOT possible by design (the bytes are gone);
//   unallocated regions and orphaned overflow chains remain readable.

/// sqlite_dbdata's column set (mirrored in namecheck's `tvf_columns`).
pub(crate) const DBDATA_COLS: [&str; 6] = ["pgno", "cell", "field", "value", "hexval", "descr"];

fn hex_of(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// Human name of a ROW-CODEC value tag (the `encode_into` codec).
fn row_tag_name(tag: u8) -> &'static str {
    match tag {
        0x00 => "null",
        0x01..=0x05 => "int",
        0x06 | 0x0A => "real",
        0x07 => "text",
        0x08 => "blob",
        0x09 => "rowid-marker",
        _ => "unknown",
    }
}

/// Decode a varint (the cell codec's unsigned LEB128) — same shape as
/// the triage harness's decoder.
fn dbdata_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for (i, &b) in buf.iter().take(9).enumerate() {
        if i == 8 {
            return Some(((v << 8) | b as u64, 9));
        }
        v = (v << 7) | (b & 0x7F) as u64;
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// Emit one (pgno, cell, field, value, hexval, descr) row.
fn dbdata_push(
    rows: &mut Vec<Vec<Value>>,
    pgno: u32,
    cell: i64,
    field: i64,
    bytes: &[u8],
    descr: String,
) {
    rows.push(vec![
        Value::Integer(pgno as i64),
        Value::Integer(cell),
        Value::Integer(field),
        Value::Blob(bytes.to_vec()),
        Value::Text(hex_of(bytes).into()),
        Value::Text(descr.into()),
    ]);
}

fn dbdata(
    ctx: &ExecContext<'_>,
    args: &[Value],
) -> Result<(Vec<&'static str>, Vec<Vec<Value>>), Error> {
    // Schema argument: only 'main' exists (the engine is single-db);
    // missing/NULL/'' default to main — anything else is SQLite's
    // "no such table" error shape.
    match args.first() {
        None | Some(Value::Null) => {}
        Some(Value::Text(t)) if t.is_empty() || t.eq_ignore_ascii_case("main") => {}
        Some(Value::Text(t)) => {
            return Err(Error::NotFound(format!("no such table: {}", t)));
        }
        Some(_) => {
            return Err(Error::semantic(
                "sqlite_dbdata: schema argument must be text",
            ))
        }
    }

    let pager = ctx.pager;
    let n_pages = pager.n_pages();

    // Freelist membership (trunk chain walk, cycle-guarded like
    // integrity's): trunk pages decode as freelist trunks; their leaf
    // entries are the zeroed freed pages.
    let mut freelist_trunks: std::collections::HashSet<u32> = Default::default();
    let mut freed_pages: std::collections::HashSet<u32> = Default::default();
    {
        let mut cur = pager.freelist_head();
        let mut hops = 0usize;
        while cur != 0 && cur < n_pages {
            if !freelist_trunks.insert(cur) {
                break; // cycle: stop, report what we have
            }
            hops += 1;
            if hops > n_pages as usize + 1 {
                break;
            }
            let page = pager.get_page(cur)?;
            let borrowed = page.lock();
            let next = u32::from_le_bytes(
                borrowed
                    .data
                    .get(..4)
                    .and_then(|s| s.try_into().ok())
                    .unwrap_or([0; 4]),
            );
            let k = u32::from_le_bytes(
                borrowed
                    .data
                    .get(4..8)
                    .and_then(|s| s.try_into().ok())
                    .unwrap_or([0; 4]),
            );
            for i in 0..k.min(1024) {
                let off = 8 + i as usize * 4;
                if let Some(e) = borrowed
                    .data
                    .get(off..off + 4)
                    .and_then(|s| s.try_into().ok())
                {
                    freed_pages.insert(u32::from_le_bytes(e));
                }
            }
            drop(borrowed);
            cur = next;
        }
    }

    let mut rows: Vec<Vec<Value>> = Vec::new();
    for pgno in 0..n_pages {
        let page = pager.get_page(pgno)?;
        let data: &[u8] = &page.lock().data;
        let hdr = if pgno == 0 { 100 } else { 0 };
        if data.len() < hdr + 12 {
            continue;
        }
        let ptype = data[hdr];
        let n_cells = u16::from_be_bytes([data[hdr + 4], data[hdr + 5]]) as usize;
        let right = u32::from_be_bytes(data[hdr + 8..hdr + 12].try_into().unwrap_or([0; 4]));
        let cell_ptr_at = |i: usize| -> Option<usize> {
            let off = hdr + 12 + i * 2;
            let p = u16::from_be_bytes(data.get(off..off + 2)?.try_into().ok()?) as usize;
            (p < data.len()).then_some(p)
        };

        if let Ok(t) = crate::storage::page::PageType::from_byte(ptype) {
            use crate::storage::page::PageType;
            let type_name = match t {
                PageType::LeafTable => "leaf-table",
                PageType::InteriorTable => "interior-table",
                PageType::LeafIndex => "leaf-index",
                PageType::InteriorIndex => "interior-index",
                PageType::Overflow => "overflow",
            };
            if t == PageType::Overflow && !freelist_trunks.contains(&pgno) {
                // Overflow page: [12..16) next (BE u32, 0 = end), [16..)
                // the chunk bytes.
                let next =
                    u32::from_be_bytes(data[hdr + 12..hdr + 16].try_into().unwrap_or([0; 4]));
                let chunk = &data[hdr + 16..];
                dbdata_push(
                    &mut rows,
                    pgno,
                    -1,
                    -1,
                    chunk,
                    format!("pgno={pgno} type=overflow next={next} len={}", chunk.len()),
                );
                continue;
            }
            // Page-header fact row.
            dbdata_push(
                &mut rows,
                pgno,
                -1,
                -1,
                &data[hdr..hdr + 12],
                format!("pgno={pgno} type={type_name} ncell={n_cells} right={right}"),
            );
            for i in 0..n_cells.min(4096) {
                let Some(ptr) = cell_ptr_at(i) else { continue };
                let cell = &data[ptr.min(data.len())..];
                match t {
                    PageType::LeafTable => {
                        // [varint rowid][varint plen][payload values..]
                        let Some((rowid, n1)) = dbdata_varint(cell) else {
                            continue;
                        };
                        dbdata_push(
                            &mut rows,
                            pgno,
                            i as i64,
                            -1,
                            &cell[..n1],
                            format!("cell={i} rowid={}", rowid as i64),
                        );
                        let Some((plen, n2)) = dbdata_varint(&cell[n1..]) else {
                            continue;
                        };
                        let payload = &cell[n1 + n2..(n1 + n2 + plen as usize).min(cell.len())];
                        let mut pos = 0usize;
                        let mut field = 0i64;
                        while pos < payload.len() && field < 2048 {
                            let tag = payload[pos];
                            let len = crate::storage::row_codec::value_encoded_len(&payload[pos..])
                                .unwrap_or(0);
                            if len == 0 || pos + len > payload.len() {
                                break;
                            }
                            let v = &payload[pos..pos + len];
                            dbdata_push(
                                &mut rows,
                                pgno,
                                i as i64,
                                field,
                                v,
                                format!(
                                    "cell={i} field={field} tag=0x{tag:02X} type={} len={len}",
                                    row_tag_name(tag)
                                ),
                            );
                            pos += len;
                            field += 1;
                        }
                    }
                    PageType::LeafIndex => {
                        // [varint rowid][varint klen][order-key values..]
                        let Some((rowid, n1)) = dbdata_varint(cell) else {
                            continue;
                        };
                        dbdata_push(
                            &mut rows,
                            pgno,
                            i as i64,
                            -1,
                            &cell[..n1],
                            format!("cell={i} rowid={}", rowid as i64),
                        );
                        let Some((klen, n2)) = dbdata_varint(&cell[n1..]) else {
                            continue;
                        };
                        let key = &cell[n1 + n2..(n1 + n2 + klen as usize).min(cell.len())];
                        emit_order_key_fields(&mut rows, pgno, i as i64, key);
                    }
                    PageType::InteriorTable => {
                        // [BE u32 child][varint separator]
                        if cell.len() < 4 {
                            continue;
                        }
                        let child = u32::from_be_bytes(cell[..4].try_into().unwrap_or([0; 4]));
                        let sep = dbdata_varint(&cell[4..]).map(|(v, _)| v as i64);
                        dbdata_push(
                            &mut rows,
                            pgno,
                            i as i64,
                            -1,
                            &cell[..cell
                                .len()
                                .min(4 + dbdata_varint(&cell[4..]).map_or(0, |(_, n)| n))],
                            format!("cell={i} child={child} sep={}", sep.unwrap_or(0)),
                        );
                    }
                    PageType::InteriorIndex => {
                        // [BE u32 child][order-key values..]
                        if cell.len() < 4 {
                            continue;
                        }
                        let child = u32::from_be_bytes(cell[..4].try_into().unwrap_or([0; 4]));
                        let key = &cell[4..];
                        dbdata_push(
                            &mut rows,
                            pgno,
                            i as i64,
                            -1,
                            &cell[..4],
                            format!("cell={i} child={child} key-bytes={}", key.len()),
                        );
                        emit_order_key_fields(&mut rows, pgno, i as i64, key);
                    }
                    PageType::Overflow => unreachable!("overflow handled above"),
                }
            }
        } else if freelist_trunks.contains(&pgno) {
            // Freelist trunk: [LE u32 next][LE u32 k][k LE u32 entries]
            let next = u32::from_le_bytes(data[..4].try_into().unwrap_or([0; 4]));
            let k = u32::from_le_bytes(data[4..8].try_into().unwrap_or([0; 4]));
            let body_len = (8 + k as usize * 4).min(data.len());
            let mut entries = Vec::new();
            for i in 0..k.min(16) {
                if let Some(e) = data.get(8 + i as usize * 4..12 + i as usize * 4) {
                    entries.push(u32::from_le_bytes(e.try_into().unwrap_or([0; 4])));
                }
            }
            dbdata_push(
                &mut rows,
                pgno,
                -1,
                -1,
                &data[..body_len],
                format!(
                    "pgno={pgno} type=freelist-trunk next={next} n={k} entries={:?}{}",
                    entries,
                    if k > 16 { "…" } else { "" }
                ),
            );
        } else if freed_pages.contains(&pgno) || ptype == 0 {
            dbdata_push(
                &mut rows,
                pgno,
                -1,
                -1,
                &[],
                format!("pgno={pgno} type=zeroed (freed)"),
            );
        } else {
            dbdata_push(
                &mut rows,
                pgno,
                -1,
                -1,
                &data[..0],
                format!("pgno={pgno} type=0x{ptype:02X} unknown"),
            );
        }
    }
    Ok((DBDATA_COLS.to_vec(), rows))
}

/// Emit the ORDER-KEY encoded fields of an index cell's key: per value
/// `[tag]` for NULL, `[tag][8B total-order key]` (+2B delta) for
/// numerics, `[tag][BE u32 len][bytes]` for text/blob.
fn emit_order_key_fields(rows: &mut Vec<Vec<Value>>, pgno: u32, cell: i64, key: &[u8]) {
    let mut pos = 0usize;
    let mut field = 0i64;
    while pos < key.len() && field < 64 {
        let tag = key[pos];
        let (len, tname) = match tag {
            0x00 => (1usize, "null"),
            0x01 => {
                // 8-byte total-order double key (+ optional 2-byte delta).
                let mut n = 9usize;
                if pos + 11 <= key.len() && key[pos + 8] & 0x80 != 0 {
                    n += 2; // large-int delta present
                }
                (n, "numeric")
            }
            0x02 | 0x03 => {
                let blen = if pos + 5 <= key.len() {
                    u32::from_be_bytes(key[pos + 1..pos + 5].try_into().unwrap_or([0; 4])) as usize
                } else {
                    0
                };
                (5 + blen, if tag == 0x02 { "text" } else { "blob" })
            }
            _ => break, // unknown tag: stop (corrupt or trailing bytes)
        };
        if pos + len > key.len() {
            break;
        }
        let v = &key[pos..pos + len];
        dbdata_push(
            rows,
            pgno,
            cell,
            field,
            v,
            format!("cell={cell} field={field} tag=0x{tag:02X} type={tname} len={len}"),
        );
        pos += len;
        field += 1;
    }
}
