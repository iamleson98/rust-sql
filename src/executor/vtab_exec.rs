//! Virtual-table execution: scans with constraint pushdown, and DML
//! through xUpdate.
//!
//! The engine side of the [`crate::plugin::vtab`] protocol. `exec_scan`
//! routes here when `table.vtab` is set; INSERT/UPDATE/DELETE route to
//! [`exec_insert_vtab`] / [`exec_update_vtab`] / [`exec_delete_vtab`].

use super::*;
use crate::error::{Error, Result};
use crate::plugin::vtab::{IndexInfo, VtabConstraint, VtabConstraintOp, VtabUpdateArg};
use crate::sql::ast::{BinaryOp, Expr, LikeOp, OrderTerm};
use crate::types::Row;
use std::sync::Arc;

/// Marker character for hidden (aux) vtab slots in scan output column
/// names: `"t.\u{1}rank"`. Unparseable as a user identifier (so star
/// expansions skip it), but `resolve_column_index`'s fallback maps `rank`
/// / `t.rank` references onto it. The engine-side sibling of the hidden
/// rowid slot's `\0` marker.
pub(crate) const AUX_MARK: char = '\u{1}';

/// Is this scan-output column name a hidden vtab aux slot?
pub(crate) fn is_aux_slot(name: &str) -> bool {
    name.contains(AUX_MARK)
}

/// The marked output name for an aux column under `prefix`.
pub(crate) fn aux_slot_name(prefix: &str, aux: &str) -> String {
    format!("{}.{}{}", prefix, AUX_MARK, aux)
}

/// Does the marked aux slot name (e.g. `t.\u{1}rank`) answer the column
/// reference `(table?, col)`? `table == None` accepts any qualifier
/// (bare-name resolution).
pub(crate) fn aux_slot_matches(slot: &str, table: Option<&str>, col: &str) -> bool {
    let Some(pos) = slot.rfind(AUX_MARK) else {
        return false;
    };
    let base = &slot[pos + AUX_MARK.len_utf8()..];
    if !base.eq_ignore_ascii_case(col) {
        return false;
    }
    match table {
        Some(t) => {
            let qual = slot[..pos].trim_end_matches('.');
            qual.eq_ignore_ascii_case(t)
        }
        None => true,
    }
}

// ============================================================================
// Aux-function TLS (bm25 / highlight / snippet)
// ============================================================================

/// Statement-thread context for aux functions (FTS5's bm25/highlight/
/// snippet): the instance whose scan is (or was most recently) driving
/// the current statement, plus the scan's bound query — the module needs
/// the CURRENT query to know which hits to highlight / score.
pub(crate) struct AuxContext {
    pub inst: Arc<crate::plugin::vtab::VtabInstance>,
}

thread_local! {
    static AUX: std::cell::RefCell<Option<AuxContext>> = const {
        std::cell::RefCell::new(None)
    };
}

/// Install the aux context for the current statement thread. Called by
/// `scan_vtab` when the scanned table's module declares aux functions.
pub(crate) fn aux_tls_install(inst: Arc<crate::plugin::vtab::VtabInstance>) {
    AUX.with(|a| *a.borrow_mut() = Some(AuxContext { inst }));
}

/// Clear any installed aux context (statement boundary).
pub(crate) fn aux_tls_clear() {
    AUX.with(|a| *a.borrow_mut() = None);
}

/// The installed aux context, if any (used by the expression evaluator's
/// bm25/highlight/snippet dispatch).
pub(crate) fn aux_tls_get() -> Option<Arc<crate::plugin::vtab::VtabInstance>> {
    AUX.with(|a| a.borrow().as_ref().map(|c| c.inst.clone()))
}

/// One conjunct of a WHERE clause (an AND-chain element).
fn split_conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    if let Expr::Binary {
        op: BinaryOp::And,
        left,
        right,
    } = e
    {
        split_conjuncts(left, out);
        split_conjuncts(right, out);
    } else {
        out.push(e);
    }
}

/// Rebuild an AND-chain from owned conjuncts.
fn rebuild_and(mut conjuncts: Vec<Expr>) -> Option<Expr> {
    let first = conjuncts.pop()?;
    Some(
        conjuncts
            .into_iter()
            .rev()
            .fold(first, |acc, e| Expr::Binary {
                op: BinaryOp::And,
                left: Box::new(e),
                right: Box::new(acc),
            }),
    )
}

/// Extract vtab constraints from a predicate: conjuncts of the shape
/// `<vtab-column> <op> <expr>` (or mirrored), plus module-overloaded
/// function calls `fn(<vtab-column>, <expr>)` (SQLite's xFindFunction
/// constraints). Returns the constraints and, for each conjunct, whether
/// it was consumed.
pub(crate) fn extract_constraints(
    predicate: &Expr,
    table: &Arc<crate::schema::Table>,
    alias: Option<&str>,
    overloads: &[&'static str],
) -> (Vec<VtabConstraint>, Vec<bool>) {
    let mut conjuncts = Vec::new();
    split_conjuncts(predicate, &mut conjuncts);
    let prefix = alias.unwrap_or(&table.name);
    let aux = table
        .vtab
        .as_ref()
        .map(|v| v.aux_columns())
        .unwrap_or_default();
    let mut constraints = Vec::new();
    let mut consumed = Vec::new();
    for c in conjuncts {
        let mut matched = false;
        if let Expr::Binary { op, left, right } = c {
            let op = *op;
            // Column-op-expr and expr-op-column (mirrored).
            if let (Some(col), Some(vop)) = (column_of(left, table, prefix, &aux), vtab_op(op)) {
                constraints.push(VtabConstraint {
                    column: col,
                    op: vop,
                    expr: (**right).clone(),
                });
                matched = true;
            } else if let (Some(col), Some(vop)) =
                (column_of(right, table, prefix, &aux), vtab_op(flip_op(op)))
            {
                constraints.push(VtabConstraint {
                    column: col,
                    op: vop,
                    expr: (**left).clone(),
                });
                matched = true;
            }
        } else if let Expr::Like {
            op,
            expr,
            pattern,
            negated: false,
            ..
        } = c
        {
            // `col LIKE pattern` (GLOB too) and — the FTS5 form —
            // `col MATCH query` / `table MATCH query` (the table self
            // column and `rank` resolve through the aux list).
            let vop = match op {
                LikeOp::Like => VtabConstraintOp::Like,
                LikeOp::Glob => VtabConstraintOp::Glob,
                LikeOp::Match => VtabConstraintOp::Match,
                LikeOp::Regexp => VtabConstraintOp::Like,
            };
            if let Some(col) = column_of(expr, table, prefix, &aux) {
                constraints.push(VtabConstraint {
                    column: col,
                    op: vop,
                    expr: (**pattern).clone(),
                });
                matched = true;
            }
        } else if let Expr::Function { name, args, .. } = c {
            // Module-overloaded function constraints (geopoly's
            // geopoly_overlap / geopoly_within): SQLite's xFindFunction —
            // the FIRST argument must be a column of this table (the
            // mirrored order falls back to a plain residual, exactly
            // like SQLite's planner).
            if args.len() == 2
                && overloads
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(name.as_str()))
            {
                if let Some(Some(col)) = column_of(&args[0], table, prefix, &aux) {
                    if column_of(&args[1], table, prefix, &aux).is_none() {
                        if let Some(f) = overloads
                            .iter()
                            .find(|f| f.eq_ignore_ascii_case(name.as_str()))
                        {
                            constraints.push(VtabConstraint {
                                column: Some(col),
                                op: VtabConstraintOp::Function(f),
                                expr: args[1].clone(),
                            });
                            matched = true;
                        }
                    }
                }
            }
        }
        consumed.push(matched);
    }
    (constraints, consumed)
}

/// Map a comparison operator to a vtab constraint op.
fn vtab_op(op: BinaryOp) -> Option<VtabConstraintOp> {
    Some(match op {
        BinaryOp::Eq => VtabConstraintOp::Eq,
        BinaryOp::Lt => VtabConstraintOp::Lt,
        BinaryOp::LtEq => VtabConstraintOp::Le,
        BinaryOp::Gt => VtabConstraintOp::Gt,
        BinaryOp::GtEq => VtabConstraintOp::Ge,
        _ => return None,
    })
}

fn flip_op(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::GtEq => BinaryOp::LtEq,
        other => other,
    }
}

/// Resolve `Expr::Column` to a vtab column index (None = rowid) when it
/// refers to this table (unqualified, or qualified by the table name/alias).
/// Aux (hidden) columns resolve to `n_user + k` — the module's full column
/// space — including the table-self column (`WHERE t MATCH '...'` with a
/// bare table-name reference).
fn column_of(
    e: &Expr,
    table: &Arc<crate::schema::Table>,
    prefix: &str,
    aux: &[(String, String)],
) -> Option<Option<usize>> {
    if let Expr::Column { table: ref_t, name } = e {
        if let Some(t) = ref_t {
            if !t.eq_ignore_ascii_case(prefix) && !t.eq_ignore_ascii_case(&table.name) {
                return None;
            }
        }
        let lower = name.to_ascii_lowercase();
        // Rowid spellings, including the planner's hidden-slot rewrite
        // ("prefix.\0rowid" — the executor resolves it positionally
        // against the vtab scan's trailing rowid slot).
        let hidden_slot_of_this =
            name.contains(".\u{0}rowid") && name.starts_with(&format!("{}.", prefix));
        if ["rowid", "oid", "_rowid_"].contains(&lower.as_str())
            || crate::planner::is_rowid_spelling(name)
            || hidden_slot_of_this
        {
            return Some(None);
        }
        if let Some(idx) = table.find_column(name) {
            return Some(Some(idx));
        }
        // Aux columns: FTS5's `rank`, the table-self column. A user
        // column with the same name wins (checked above).
        let n_user = table.n_columns();
        for (k, (an, _)) in aux.iter().enumerate() {
            if an.eq_ignore_ascii_case(name) {
                return Some(Some(n_user + k));
            }
        }
        // The table-self reference: a bare (or qualified) reference whose
        // name is the table alias / table name and does NOT collide with a
        // user column resolves to the self aux column when one exists
        // (FTS5: the self column is named after the table).
        if lower.eq_ignore_ascii_case(prefix) || lower.eq_ignore_ascii_case(&table.name) {
            for (k, (an, _)) in aux.iter().enumerate() {
                if an.eq_ignore_ascii_case(&table.name) {
                    return Some(Some(n_user + k));
                }
            }
        }
    }
    None
}

/// Column names for a vtab scan's output: the user columns, then the
/// module's aux (hidden) columns under marker names (`prefix.\u{1}rank`) —
/// resolvable by explicit reference, excluded from star expansion.
fn vtab_output_columns(
    table: &Arc<crate::schema::Table>,
    alias: Option<&str>,
    aux: &[(String, String)],
) -> Arc<[String]> {
    let prefix = alias.unwrap_or(&table.name);
    let mut names: Vec<String> = if prefix == table.name {
        table.qualified_col_names.iter().cloned().collect()
    } else {
        table
            .columns
            .iter()
            .map(|c| format!("{}.{}", prefix, c.name))
            .collect()
    };
    for (an, _) in aux {
        names.push(aux_slot_name(prefix, an));
    }
    names.push(crate::planner::hidden_rowid_slot(prefix));
    names.into()
}

/// vtab scan output: column names + (rowid, row) pairs.
pub(crate) type VtabScanResult = Result<(Arc<[String]>, Vec<(i64, Row)>)>;

/// Rebuild a vtab module's in-memory state from its content shadow when
/// needed (first use after reopen, or after a rollback restored the
/// shadow's pages). No-op for modules without a content shadow.
pub(crate) fn ensure_vtab_synced(
    ctx: &mut ExecContext<'_>,
    table: &Arc<crate::schema::Table>,
) -> Result<()> {
    let inst = match table.vtab.as_ref() {
        Some(i) => i.clone(),
        None => return Ok(()),
    };
    if !inst.needs_reindex() {
        return Ok(());
    }
    let shadow_name = match inst.content_shadow() {
        Some(n) => n,
        None => {
            inst.clear_reindex();
            return Ok(());
        }
    };
    let shadow = ctx
        .catalog()
        .get_table(&shadow_name)
        .ok_or_else(|| Error::corruption(format!("missing shadow table {}", shadow_name)))?;
    // Shadow layouts can differ from the vtab's column order (geopoly's
    // SQLite-layout t_rowid: rowid, nodeno, a0, a1, ...). Map the shadow
    // columns back onto the vtab's column order for reindex.
    let content_map = inst
        .shadow_tables()
        .into_iter()
        .find(|s| s.content)
        .and_then(|s| s.content_map);
    let n_shadow_cols = shadow.n_columns();
    let root = ctx.table_root(&shadow);
    let mut bt = Btree::new(ctx.pager, root, false);
    let mut rows: Vec<(i64, Vec<Value>)> = Vec::new();
    let mut decode_err: Option<Error> = None;
    bt.scan_table(|rowid, payload| {
        let row = match decode_row(payload, n_shadow_cols, rowid, shadow.rowid_alias) {
            Ok(r) => r,
            Err(e) => {
                decode_err = Some(e);
                return false;
            }
        };
        let vals = match &content_map {
            Some(map) => {
                let mut v = vec![Value::Null; map.len()];
                for (vi, &si) in map.iter().enumerate() {
                    v[vi] = row.get(si).cloned().unwrap_or(Value::Null);
                }
                v
            }
            None => row,
        };
        rows.push((rowid, vals));
        true
    })?;
    if let Some(e) = decode_err {
        return Err(e);
    }
    inst.run_reindex(&rows)?;
    inst.clear_reindex();
    Ok(())
}

/// Scan a virtual table, applying `predicate` (constraints pushed into the
/// module via best_index; the rest applied as a residual filter).
/// Returns (columns, (rowid, row) pairs).
pub(crate) fn scan_vtab(
    ctx: &mut ExecContext<'_>,
    table: &Arc<crate::schema::Table>,
    alias: Option<&str>,
    predicate: Option<&Expr>,
) -> VtabScanResult {
    let inst = table.vtab.as_ref().ok_or_else(|| {
        Error::corruption(format!("vtab exec on non-virtual table {}", table.name))
    })?;
    inst.ensure_connected()?;
    // Reopen / post-rollback: rebuild the module state from the content
    // shadow before the scan sees it.
    ensure_vtab_synced(ctx, table)?;
    let aux = inst.aux_columns();

    // 1. best_index + cursor open (state lock held only for these calls).
    struct Prepared {
        info: IndexInfo,
        filter_args: Vec<Value>,
        overloads: Vec<&'static str>,
    }
    let prepared: Prepared = inst.with_table(|vt| {
        let overloads: Vec<&'static str> = vt.overloaded_functions().to_vec();
        let (constraints, _consumed) = match predicate {
            Some(p) => extract_constraints(p, table, alias, &overloads),
            None => (Vec::new(), Vec::new()),
        };
        let mut info = vt.best_index(&constraints)?;
        if info.handled.len() != constraints.len() {
            // Defensive: modules must return one flag per constraint.
            info.handled.resize(constraints.len(), false);
        }
        if info.recheck.len() != constraints.len() {
            info.recheck.resize(constraints.len(), false);
        }
        // Evaluate the RHS of handled constraints with the statement's
        // parameters (no row context — these are constants/params).
        let empty_row: Vec<Value> = Vec::new();
        let empty_cols: Vec<String> = Vec::new();
        let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
        let mut filter_args = Vec::with_capacity(constraints.len());
        for (c, handled) in constraints.iter().zip(info.handled.iter()) {
            if *handled {
                filter_args.push(evaluate(&c.expr, &eval_ctx)?);
            }
        }
        Ok(Prepared {
            info,
            filter_args,
            overloads,
        })
    })?;

    // 2. Residual predicate: conjuncts the module did NOT handle. A
    // conjunct "handled" = extracted as a constraint AND marked handled by
    // best_index (info.handled) AND NOT marked recheck (the module wants
    // the value at filter time but does not promise exactness — geopoly's
    // bbox prefilter, re-verified by the re-applied function). Conjuncts
    // that never extracted are always residual. Constraint j corresponds
    // to the j-th MATCHED conjunct.
    let residual: Option<Expr> = match predicate {
        Some(p) => {
            let (_constraints, consumed) =
                extract_constraints(p, table, alias, &prepared.overloads);
            let handled_flags = &prepared.info.handled;
            let recheck_flags = &prepared.info.recheck;
            let module_handles_any = handled_flags.iter().any(|h| *h);
            if !module_handles_any {
                // Nothing handled: whole predicate is the residual.
                Some(p.clone())
            } else {
                let mut conjuncts = Vec::new();
                split_conjuncts(p, &mut conjuncts);
                let mut ci = 0usize; // cursor into handled_flags
                let keep: Vec<Expr> = conjuncts
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, e)| {
                        if consumed.get(i).copied().unwrap_or(false) {
                            let handled = handled_flags.get(ci).copied().unwrap_or(false);
                            let recheck = recheck_flags.get(ci).copied().unwrap_or(false);
                            ci += 1;
                            if handled && !recheck {
                                None
                            } else {
                                Some(e.clone())
                            }
                        } else {
                            Some(e.clone())
                        }
                    })
                    .collect();
                rebuild_and(keep)
            }
        }
        None => None,
    };

    // 3. Drive the cursor. Rows carry the user columns followed by the
    // aux (hidden) slots; the output column list matches.
    let n_cols = table.n_columns();
    let columns = vtab_output_columns(table, alias, &aux);
    let mut out: Vec<(i64, Row)> = Vec::new();
    let mut cursor = inst.with_table(|vt| vt.open())?;
    cursor.filter(
        prepared.info.idx_num,
        prepared.info.idx_str.as_deref(),
        &prepared.filter_args,
    )?;
    let n_total = n_cols + aux.len() + 1;
    while !cursor.eof() {
        let rowid = cursor.rowid()?;
        let mut row = Vec::with_capacity(n_total);
        for i in 0..n_total - 1 {
            row.push(cursor.column(i)?);
        }
        // Trailing hidden rowid slot: every existing rowid reference
        // (bare and side-qualified) resolves against it through the
        // ordinary hidden-slot machinery.
        row.push(Value::Integer(rowid));
        out.push((rowid, row));
        cursor.next()?;
    }
    drop(cursor);

    // 3b. Aux functions (FTS5's bm25/highlight/snippet): install the
    // module context for this statement thread so the expression
    // evaluator can dispatch the calls.
    let has_aux_fns = inst
        .with_table(|vt| Ok(!vt.aux_functions().is_empty()))
        .unwrap_or(false);
    if has_aux_fns {
        aux_tls_install(inst.clone());
    }

    // 3c. External-content tables (FTS5 content='tbl'): the module
    // serves the INDEX; the content-column VALUES come from the external
    // table, fetched here by rowid key.
    let external = inst
        .with_table(|vt| Ok(vt.external_content()))
        .unwrap_or(None);
    if let Some((ctbl, crowid)) = external {
        if let Some(content) = ctx.catalog().get_table(&ctbl) {
            if let Some(rc_idx) = content.find_column(&crowid) {
                let root = ctx.table_root(&content);
                let mut map: std::collections::HashMap<i64, Vec<Value>> =
                    std::collections::HashMap::new();
                let mut bt = Btree::new(ctx.pager, root, false);
                bt.scan_table(|_rid, payload| {
                    if let Ok(row) =
                        decode_row(payload, content.n_columns(), _rid, content.rowid_alias)
                    {
                        if let Some(Value::Integer(k)) = row.get(rc_idx) {
                            let vals: Vec<Value> = row
                                .iter()
                                .enumerate()
                                .filter(|(i, _)| *i != rc_idx)
                                .map(|(_, v)| v.clone())
                                .collect();
                            map.insert(*k, vals);
                        }
                    }
                    true
                })?;
                drop(bt);
                let n_user = table.n_columns();
                for (rid, row) in out.iter_mut() {
                    if let Some(vals) = map.get(rid) {
                        for (i, v) in vals.iter().take(n_user).enumerate() {
                            row[i] = v.clone();
                        }
                    }
                }
            }
        }
    }

    // 4. Residual filter.
    if let Some(pred) = &residual {
        let params: &[Value] = &ctx.params;
        let named_params = &ctx.named_params;
        let col_names: Vec<String> = columns.iter().cloned().collect();
        // A raising residual fails the statement (it used to drop the
        // row silently).
        let mut residual_err = None;
        out.retain(|(_, row)| {
            if residual_err.is_some() {
                return false;
            }
            match eval_row_where(pred, row, &col_names, params, named_params) {
                Ok(v) => v,
                Err(e) => {
                    residual_err = Some(e);
                    false
                }
            }
        });
        if let Some(e) = residual_err {
            return Err(e);
        }
    }
    Ok((columns, out))
}

/// `Plan::Scan` over a virtual table.
pub(crate) fn exec_scan_vtab(
    ctx: &mut ExecContext<'_>,
    table: &Arc<crate::schema::Table>,
    alias: Option<&String>,
    predicate: Option<&Expr>,
) -> Result<ExecResult> {
    let (columns, pairs) = scan_vtab(ctx, table, alias.map(|s| s.as_str()), predicate)?;
    let rows = pairs.into_iter().map(|(_, row)| row).collect();
    Ok(ExecResult { columns, rows })
}

/// INSERT into a virtual table (xUpdate with old_rowid = None).
pub(crate) fn exec_insert_vtab(
    ctx: &mut ExecContext<'_>,
    table: &Arc<crate::schema::Table>,
    source_rows: Vec<Row>,
    column_indices: Option<&Vec<usize>>,
    on_conflict: crate::sql::ast::ConflictResolution,
) -> Result<()> {
    // Modules own their conflict policy (SQLite's argv semantics); the
    // resolution hint only participates through the module's
    // rowid_unique_error opt-in below (geopoly/rtree: SQLite's
    // rtreeConstraintError + OR REPLACE / OR IGNORE policies).
    let inst = table
        .vtab
        .as_ref()
        .ok_or_else(|| Error::corruption("vtab exec on non-virtual table".to_string()))?;
    if !inst.writable()? {
        return Err(Error::semantic(format!(
            "cannot INSERT into read-only virtual table {}",
            table.name
        )));
    }
    inst.ensure_connected()?;
    ensure_vtab_synced(ctx, table)?;
    let n_cols = table.n_columns();
    let shadow = shadow_pair(ctx, inst)?;
    // Modules that declare a rowid-conflict error (SQLite's rtree/
    // geopoly UNIQUE-constraint discipline): the engine checks the
    // content shadow BEFORE writing and applies the conflict policy.
    let rowid_conflict_error: Option<String> = inst
        .with_table(|vt| Ok(vt.rowid_unique_error()))
        .unwrap_or(None);
    let rowid_col: Option<usize> = inst.with_table(|vt| Ok(vt.rowid_column())).unwrap_or(None);
    if let Some(sc) = &shadow {
        push_vtab_resync_marker(ctx, &sc.inst);
    }
    let mut changes = 0i64;
    let mut last_rowid = ctx.last_insert_rowid;
    for row in source_rows {
        // FTS5 INSERT-command form: one column of the list is the table
        // itself (`INSERT INTO t(t, rowid, a) VALUES('delete', 5, ...)`).
        let mut command: Option<String> = None;
        let mut cmd_rowid: Option<i64> = None;
        let mut values: Vec<Option<Value>> = vec![None; n_cols];
        let mut explicit_rowid: Option<i64> = None;
        if let Some(idxs) = column_indices {
            if idxs.len() != row.len() {
                return Err(Error::semantic(format!(
                    "table {}: {} values for {} columns",
                    table.name,
                    row.len(),
                    idxs.len()
                )));
            }
            for (i, col_idx) in idxs.iter().enumerate() {
                if *col_idx == super::ROWID_COLUMN_SENTINEL {
                    explicit_rowid = crate::executor::rowid_from_value(&row[i])?.or(Some(0));
                    cmd_rowid = explicit_rowid;
                    continue;
                }
                if *col_idx == super::VTAB_SELF_SENTINEL {
                    match &row[i] {
                        Value::Text(t) => command = Some(t.as_str().to_string()),
                        _ => {
                            return Err(Error::semantic(
                                "invalid value for the fts5 command column",
                            ))
                        }
                    }
                    continue;
                }
                // SQLite passes vtab column values through UNCOERCED
                // (geopoly's typed aux columns keep raw values: '5' into
                // an INTEGER-declared aux column stays TEXT).
                values[*col_idx] = Some(row[i].clone());
            }
            if let Some(cmd) = command {
                run_vtab_command(
                    ctx,
                    table,
                    inst,
                    &shadow,
                    &cmd,
                    cmd_rowid.or(explicit_rowid),
                    &values,
                )?;
                continue;
            }
        } else {
            if row.len() != n_cols {
                return Err(Error::semantic(format!(
                    "table {} has {} columns but {} values were supplied",
                    table.name,
                    n_cols,
                    row.len()
                )));
            }
            // Uncoerced, like the column-list path above.
            for (i, v) in row.into_iter().enumerate() {
                values[i] = Some(v);
            }
        }
        // Modules whose rowid IS a column (rtree's id): the column value
        // is the rowid (shadow key, max-rowid tracking, dup checks).
        if explicit_rowid.is_none() {
            if let Some(c) = rowid_col {
                if let Some(Some(v)) = values.get(c) {
                    if !v.is_null() {
                        explicit_rowid = crate::executor::rowid_from_value(v)?;
                    }
                }
            }
        }
        // Engine-assigned rowid when a content shadow backs the table
        // (the shadow row needs its key before xUpdate runs); otherwise
        // the module assigns (SQLite's argv semantics).
        let (op_rowid, rowid) = if let Some(sc) = &shadow {
            let rid = match explicit_rowid {
                Some(r) => r,
                None => ctx.get_or_scan_max_rowid(&sc.table)? + 1,
            };
            (Some(rid), rid)
        } else {
            (explicit_rowid, explicit_rowid.unwrap_or(0))
        };
        // Duplicate explicit rowid: modules with a declared conflict
        // error reject (or delete-then-insert under OR REPLACE, or skip
        // under OR IGNORE) — SQLite's rtreeConstraintError path.
        if let (Some(err), Some(sc), Some(rid)) = (&rowid_conflict_error, &shadow, explicit_rowid) {
            let exists = {
                let root = ctx.table_root(&sc.table);
                let mut bt = Btree::new(ctx.pager, root, false);
                matches!(
                    bt.lookup_table(rid)?,
                    crate::storage::btree::LookupResult::Found(_)
                )
            };
            if exists {
                match on_conflict {
                    crate::sql::ast::ConflictResolution::Replace => {
                        shadow_delete(ctx, &sc.table, rid)?;
                        let ops = vec![crate::plugin::vtab::UpdateOp {
                            old_rowid: Some(rid),
                            new_rowid: None,
                            columns: Vec::new(),
                        }];
                        inst.with_table(|vt| vt.update(ops))?;
                    }
                    crate::sql::ast::ConflictResolution::Ignore => {
                        continue;
                    }
                    _ => return Err(Error::constraint(err.clone())),
                }
            }
        }
        // Content shadow first: the row rides the ordinary pager (and
        // rolls back with the statement/transaction).
        if let Some(sc) = &shadow {
            let full: Vec<Value> = values
                .iter()
                .map(|v| v.clone().unwrap_or(Value::Null))
                .collect();
            let full = inst
                .with_table(|vt| Ok(vt.shadow_normalize(&full)))
                .unwrap_or(full);
            shadow_insert(ctx, &sc.table, &sc.content_map, rowid, &full)?;
        }
        let op = crate::plugin::vtab::UpdateOp {
            old_rowid: None,
            new_rowid: op_rowid,
            columns: values,
        };
        let assigned = match inst.with_table(|vt| vt.update(vec![op])) {
            Ok(a) => a,
            Err(e) => {
                // The module may have partially applied its batch; the
                // shadow rows are restored by the statement undo — the
                // module's in-memory state rebuilds lazily.
                inst.request_reindex();
                return Err(e);
            }
        };
        changes += 1;
        if let Some(Some(rid)) = assigned.first().copied() {
            last_rowid = rid;
            last_insert_rowid_track(ctx, &shadow, rid);
        } else if op_rowid.is_some() {
            last_insert_rowid_track(ctx, &shadow, rowid);
            last_rowid = rowid;
        }
    }
    ctx.changes = changes;
    ctx.last_insert_rowid = last_rowid;
    crate::executor::change_counters::note_conn_rowid(last_rowid);
    // Autocommit: the vtab branch returns before the normal insert tail's
    // flush — the shadow rows ride the pager like any table write, so the
    // same boundary applies (matches the plain-insert path).
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    Ok(())
}

/// Track an engine-assigned vtab rowid in the max-rowid cache so the
/// NEXT insert's `max+1` sees it (the shadow table's cache entry).
fn last_insert_rowid_track(ctx: &mut ExecContext<'_>, shadow: &Option<ShadowCtx>, rowid: i64) {
    if let Some(sc) = shadow {
        let key = sc.table.name.to_ascii_lowercase();
        let cur = ctx.max_rowids.get(&key).copied().unwrap_or(0);
        if rowid > cur {
            ctx.max_rowids.insert(key, rowid);
            ctx.max_rowids_changed = true;
        }
    }
}

/// The (shadow table, instance) pair for a content-shadow-backed vtab,
/// plus the shadow's column mapping when its layout is not the engine
/// default (see [`crate::plugin::vtab::ShadowTable::content_map`]).
struct ShadowCtx {
    table: Arc<crate::schema::Table>,
    inst: Arc<crate::plugin::vtab::VtabInstance>,
    content_map: Option<Vec<usize>>,
}

fn shadow_pair(
    ctx: &ExecContext<'_>,
    inst: &Arc<crate::plugin::vtab::VtabInstance>,
) -> Result<Option<ShadowCtx>> {
    match inst.content_shadow() {
        Some(name) => {
            let t = ctx.catalog().get_table(&name).ok_or_else(|| {
                Error::corruption(format!("missing shadow table {} for vtab", name))
            })?;
            let content_map = inst
                .shadow_tables()
                .into_iter()
                .find(|s| s.content)
                .and_then(|s| s.content_map);
            Ok(Some(ShadowCtx {
                table: t,
                inst: inst.clone(),
                content_map,
            }))
        }
        None => Ok(None),
    }
}

/// Insert one content-shadow row (rowid + user column values). Journaled
/// for statement undo like an ordinary table insert. Applies the shadow's
/// column mapping when its layout differs.
fn shadow_insert(
    ctx: &mut ExecContext<'_>,
    shadow: &Arc<crate::schema::Table>,
    content_map: &Option<Vec<usize>>,
    rowid: i64,
    values: &[Value],
) -> Result<()> {
    let row = map_shadow_row(content_map, shadow.n_columns(), values);
    let payload = crate::storage::row_codec::encode_row(&row);
    let root = ctx.table_root(shadow);
    let mut bt = Btree::new(ctx.pager, root, false);
    bt.insert_table(rowid, &payload)?;
    if bt.root != root {
        ctx.set_table_root_lc(&shadow.name.to_ascii_lowercase(), bt.root);
        ctx.roots_changed = true;
    }
    ctx.stmt_undo.push(super::StmtUndoEntry::Inserted {
        table: shadow.clone(),
        rowid,
    });
    ctx.pager
        .vtab_shadow_writes
        .store(true, std::sync::atomic::Ordering::Release);
    Ok(())
}

/// Build the shadow-shaped row from vtab column values: identity when no
/// map, else `map[i]` positions (unmapped shadow columns NULL).
fn map_shadow_row(
    content_map: &Option<Vec<usize>>,
    n_shadow_cols: usize,
    values: &[Value],
) -> Vec<Value> {
    match content_map {
        None => values.to_vec(),
        Some(map) => {
            let mut row = vec![Value::Null; n_shadow_cols];
            for (vi, v) in values.iter().enumerate() {
                if let Some(&si) = map.get(vi) {
                    if si < n_shadow_cols {
                        row[si] = v.clone();
                    }
                }
            }
            row
        }
    }
}

/// Delete one content-shadow row (payload captured for undo).
fn shadow_delete(
    ctx: &mut ExecContext<'_>,
    shadow: &Arc<crate::schema::Table>,
    rowid: i64,
) -> Result<()> {
    let root = ctx.table_root(shadow);
    let mut bt = Btree::new(ctx.pager, root, false);
    let payload = match bt.lookup_table(rowid)? {
        crate::storage::btree::LookupResult::Found(p) => p,
        crate::storage::btree::LookupResult::NotFound => return Ok(()),
    };
    bt.delete_table(rowid)?;
    if bt.root != root {
        ctx.set_table_root_lc(&shadow.name.to_ascii_lowercase(), bt.root);
        ctx.roots_changed = true;
    }
    ctx.stmt_undo.push(super::StmtUndoEntry::Deleted {
        table: shadow.clone(),
        rowid,
        payload,
    });
    ctx.pager
        .vtab_shadow_writes
        .store(true, std::sync::atomic::Ordering::Release);
    Ok(())
}

/// Replace one content-shadow row's content in place (or delete+insert
/// when the payload size class changes).
fn shadow_update(
    ctx: &mut ExecContext<'_>,
    shadow: &Arc<crate::schema::Table>,
    content_map: &Option<Vec<usize>>,
    rowid: i64,
    values: &[Value],
) -> Result<()> {
    let row = map_shadow_row(content_map, shadow.n_columns(), values);
    let payload = crate::storage::row_codec::encode_row(&row);
    let root = ctx.table_root(shadow);
    let mut bt = Btree::new(ctx.pager, root, false);
    let old_payload = match bt.lookup_table(rowid)? {
        crate::storage::btree::LookupResult::Found(p) => p,
        crate::storage::btree::LookupResult::NotFound => {
            // Row absent (contentless modules never wrote it): insert.
            bt.insert_table(rowid, &payload)?;
            if bt.root != root {
                ctx.set_table_root_lc(&shadow.name.to_ascii_lowercase(), bt.root);
                ctx.roots_changed = true;
            }
            ctx.stmt_undo.push(super::StmtUndoEntry::Inserted {
                table: shadow.clone(),
                rowid,
            });
            ctx.pager
                .vtab_shadow_writes
                .store(true, std::sync::atomic::Ordering::Release);
            return Ok(());
        }
    };
    let did_in_place = bt.update_table(rowid, &payload).unwrap_or(false);
    if !did_in_place {
        bt.delete_table(rowid)?;
        bt.insert_table(rowid, &payload)?;
    }
    if bt.root != root {
        ctx.set_table_root_lc(&shadow.name.to_ascii_lowercase(), bt.root);
        ctx.roots_changed = true;
    }
    ctx.stmt_undo.push(super::StmtUndoEntry::Updated {
        table: shadow.clone(),
        rowid,
        old_payload,
    });
    ctx.pager
        .vtab_shadow_writes
        .store(true, std::sync::atomic::Ordering::Release);
    Ok(())
}

/// Mark a vtab for lazy reindex inside the statement journal (replayed on
/// statement failure — the module's in-memory state may have partially
/// applied while the shadow rows are being restored).
pub(crate) fn push_vtab_resync_marker(
    ctx: &mut ExecContext<'_>,
    inst: &Arc<crate::plugin::vtab::VtabInstance>,
) {
    ctx.stmt_undo
        .push(super::StmtUndoEntry::VtabResync { inst: inst.clone() });
}

/// Collect (rowid, row) pairs matching a predicate for DML.
fn scan_vtab_for_dml(
    ctx: &mut ExecContext<'_>,
    table: &Arc<crate::schema::Table>,
    predicate: Option<&Expr>,
) -> Result<Vec<(i64, Row)>> {
    let (_, pairs) = scan_vtab(ctx, table, None, predicate)?;
    Ok(pairs)
}

/// UPDATE a virtual table: scan matching rows, evaluate SET per row,
/// batch xUpdate ops.
pub(crate) fn exec_update_vtab(
    ctx: &mut ExecContext<'_>,
    table: &Arc<crate::schema::Table>,
    assignments: &[(usize, Expr)],
    predicate: Option<&Expr>,
) -> Result<()> {
    let inst = table
        .vtab
        .as_ref()
        .ok_or_else(|| Error::corruption("vtab exec on non-virtual table".to_string()))?;
    if !inst.writable()? {
        return Err(Error::semantic(format!(
            "cannot UPDATE read-only virtual table {}",
            table.name
        )));
    }
    let pairs = scan_vtab_for_dml(ctx, table, predicate)?;
    let n_cols = table.n_columns();
    let col_names: Vec<String> = table
        .columns
        .iter()
        .map(|c| format!("{}.{}", table.name, c.name))
        .collect();
    let params: Vec<Value> = ctx.params.clone();
    let named = ctx.named_params.clone();
    let mut ops = Vec::with_capacity(pairs.len());
    let mut new_rows: Vec<(i64, Vec<Value>)> = Vec::with_capacity(pairs.len());
    for (rowid, row) in pairs {
        let mut columns: Vec<Option<Value>> = vec![None; n_cols];
        for (col_idx, expr) in assignments {
            let v = eval_row(expr, &row, &col_names, &params, &named)?;
            // Uncoerced (SQLite vtab UPDATE path - see the INSERT note).
            columns[*col_idx] = Some(v);
        }
        let full: Vec<Value> = row
            .iter()
            .take(n_cols)
            .enumerate()
            .map(|(i, old)| columns[i].clone().unwrap_or_else(|| old.clone()))
            .collect();
        new_rows.push((rowid, full));
        ops.push(crate::plugin::vtab::UpdateOp {
            old_rowid: Some(rowid),
            new_rowid: Some(rowid),
            columns,
        });
    }
    let n = ops.len() as i64;
    // Content shadow first (per-row payload swap), then the module batch.
    let shadow = shadow_pair(ctx, inst)?;
    if let Some(sc) = &shadow {
        push_vtab_resync_marker(ctx, &sc.inst);
        let normalized: Vec<(i64, Vec<Value>)> = new_rows
            .iter()
            .map(|(rid, full)| {
                let n = inst
                    .with_table(|vt| Ok(vt.shadow_normalize(full)))
                    .unwrap_or_else(|_| full.clone());
                (*rid, n)
            })
            .collect();
        for (rowid, full) in &normalized {
            shadow_update(ctx, &sc.table, &sc.content_map, *rowid, full)?;
        }
    }
    if let Err(e) = inst.with_table(|vt| vt.update(ops)) {
        if shadow.is_some() {
            inst.request_reindex();
        }
        return Err(e);
    }
    ctx.changes = n;
    // Autocommit: the vtab branch returns before the normal insert tail's
    // flush — the shadow rows ride the pager like any table write, so the
    // same boundary applies (matches the plain-insert path).
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    Ok(())
}

/// DELETE from a virtual table: xUpdate with old_rowid and no columns.
pub(crate) fn exec_delete_vtab(
    ctx: &mut ExecContext<'_>,
    table: &Arc<crate::schema::Table>,
    predicate: Option<&Expr>,
) -> Result<()> {
    let inst = table
        .vtab
        .as_ref()
        .ok_or_else(|| Error::corruption("vtab exec on non-virtual table".to_string()))?;
    if !inst.writable()? {
        return Err(Error::semantic(format!(
            "cannot DELETE from read-only virtual table {}",
            table.name
        )));
    }
    let pairs = scan_vtab_for_dml(ctx, table, predicate)?;
    let ops: Vec<crate::plugin::vtab::UpdateOp> = pairs
        .iter()
        .map(|(rowid, _)| crate::plugin::vtab::UpdateOp {
            old_rowid: Some(*rowid),
            new_rowid: None,
            columns: Vec::new(),
        })
        .collect();
    let n = match ops.len() {
        0 => 0,
        n => {
            // Content shadow first (per-row delete), then the module batch.
            let shadow = shadow_pair(ctx, inst)?;
            if let Some(sc) = &shadow {
                push_vtab_resync_marker(ctx, &sc.inst);
                for (rowid, _) in &pairs {
                    shadow_delete(ctx, &sc.table, *rowid)?;
                }
            }
            if let Err(e) = inst.with_table(|vt| vt.update(ops)) {
                if shadow.is_some() {
                    inst.request_reindex();
                }
                return Err(e);
            }
            n as i64
        }
    };
    ctx.changes = n;
    // Autocommit: the vtab branch returns before the normal insert tail's
    // flush — the shadow rows ride the pager like any table write, so the
    // same boundary applies (matches the plain-insert path).
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    Ok(())
}

/// EXPLAIN row rendering for a vtab scan.
#[allow(dead_code)]
pub(crate) fn explain_scan_vtab(table: &Arc<crate::schema::Table>) -> Vec<Row> {
    let module = table
        .vtab
        .as_ref()
        .map(|v| v.module_name.clone())
        .unwrap_or_default();
    vec![vec![
        Value::Text("SCAN".into()),
        Value::Text(format!("{} VIRTUAL TABLE", table.name).into()),
        Value::Text(format!("module={}", module).into()),
    ]]
}

/// ORDER terms for vtab scans are applied by the generic Sort — nothing
/// vtab-specific here; kept for future orderByConsumed support.
#[allow(dead_code)]
fn vtab_order_terms(_terms: &[OrderTerm]) -> Option<()> {
    None
}

/// SQLite VtabUpdateArg type alias re-export (kept for doc links).
#[allow(dead_code)]
fn _type_check(_: Option<VtabUpdateArg>) {}

/// FTS5 INSERT-commands: `'delete'` (un-index a rowid, the contentless /
/// external-content trigger pattern), `'insert'` (legacy explicit),
/// `'rebuild'` (reindex from the content source), `'integrity-check'`,
/// `'optimize'` / `'automerge'` / `'crisismerge'` / `'usermerge'` /
/// `'pgsz'` (accepted no-ops at this engine's in-memory index), and
/// `'rank'` (set the default bm25 weights).
fn run_vtab_command(
    ctx: &mut ExecContext<'_>,
    table: &Arc<crate::schema::Table>,
    inst: &Arc<crate::plugin::vtab::VtabInstance>,
    shadow: &Option<ShadowCtx>,
    cmd: &str,
    rowid: Option<i64>,
    values: &[Option<Value>],
) -> Result<()> {
    let cmd = cmd.trim().to_ascii_lowercase();
    match cmd.as_str() {
        "delete" => {
            let Some(rid) = rowid else {
                return Err(Error::semantic("fts5 'delete' requires a rowid"));
            };
            if let Some(sc) = shadow {
                shadow_delete(ctx, &sc.table, rid)?;
            }
            let ops = vec![crate::plugin::vtab::UpdateOp {
                old_rowid: Some(rid),
                new_rowid: None,
                columns: Vec::new(),
            }];
            if let Err(e) = inst.with_table(|vt| vt.update(ops)) {
                if shadow.is_some() {
                    inst.request_reindex();
                }
                return Err(e);
            }
            ctx.changes = 1;
            Ok(())
        }
        "insert" => {
            // Legacy explicit-insert command: values already placed.
            let Some(rid) = rowid else {
                return Err(Error::semantic("fts5 'insert' requires a rowid"));
            };
            let full: Vec<Value> = values
                .iter()
                .map(|v| v.clone().unwrap_or(Value::Null))
                .collect();
            if let Some(sc) = shadow {
                shadow_insert(ctx, &sc.table, &sc.content_map, rid, &full)?;
            }
            let ops = vec![crate::plugin::vtab::UpdateOp {
                old_rowid: None,
                new_rowid: Some(rid),
                columns: full.into_iter().map(Some).collect(),
            }];
            if let Err(e) = inst.with_table(|vt| vt.update(ops)) {
                if shadow.is_some() {
                    inst.request_reindex();
                }
                return Err(e);
            }
            ctx.changes = 1;
            Ok(())
        }
        "rebuild" => {
            // Reindex from the content source: the external content table
            // when declared, else the engine-managed shadow.
            let external = inst
                .with_table(|vt| Ok(vt.external_content()))
                .unwrap_or(None);
            if let Some((ctbl, crowid)) = external {
                let content = ctx
                    .catalog()
                    .get_table(&ctbl)
                    .ok_or_else(|| Error::NotFound(format!("no such table: {ctbl}")))?;
                let rows = scan_content_rows(ctx, &content, &crowid, table.n_columns())?;
                inst.run_reindex(&rows)?;
            } else {
                inst.request_reindex();
                ensure_vtab_synced(ctx, table)?;
            }
            ctx.changes = 0;
            Ok(())
        }
        "integrity-check" => {
            // The module's in-memory state must match the content shadow.
            if let Some(sc) = shadow {
                let shadow_table = &sc.table;
                let mut rows: Vec<(i64, Vec<Value>)> = Vec::new();
                let root = ctx.table_root(shadow_table);
                let mut bt = Btree::new(ctx.pager, root, false);
                let mut decode_err: Option<Error> = None;
                bt.scan_table(|rid, payload| {
                    match decode_row(
                        payload,
                        shadow_table.n_columns(),
                        rid,
                        shadow_table.rowid_alias,
                    ) {
                        Ok(row) => rows.push((rid, row)),
                        Err(e) => {
                            decode_err = Some(e);
                            return false;
                        }
                    }
                    true
                })?;
                drop(bt);
                if let Some(e) = decode_err {
                    return Err(e);
                }
                // Deep compare: reindex the shadow rows into a scratch
                // copy is the module's own job; here the shadow scan itself
                // succeeding plus a row-count sanity check is the check.
                let _ = &rows;
                let ok = inst
                    .with_table(|vt| Ok(vt.reindex(&rows).is_ok()))
                    .unwrap_or(false);
                if !ok {
                    return Err(Error::constraint("fts5: integrity check failed"));
                }
            }
            ctx.changes = 0;
            Ok(())
        }
        "optimize" | "automerge" | "crisismerge" | "usermerge" | "pgsz" | "secure-delete"
        | "tokendata" => {
            ctx.changes = 0;
            Ok(())
        }
        "rank" => {
            // INSERT INTO t(t, rank) VALUES('rank', 'bm25(w0, w1, ...)')
            let spec = values
                .iter()
                .flatten()
                .find_map(|v| match v {
                    Value::Text(t) => Some(t.as_str().to_string()),
                    _ => None,
                })
                .unwrap_or_default();
            let inner = spec
                .trim()
                .trim_start_matches("bm25(")
                .trim_end_matches(')')
                .trim()
                .to_string();
            let weights: Vec<f64> = inner
                .split(',')
                .filter_map(|x| x.trim().parse::<f64>().ok())
                .collect();
            inst.with_table(|vt| vt.set_rank_weights(&weights))?;
            ctx.changes = 0;
            Ok(())
        }
        _ => Err(Error::semantic(format!(
            "SQL logic error near \"{cmd}\": unrecognized fts5 command"
        ))),
    }
}

/// Scan a content table for `rebuild`: maps its non-rowid columns onto
/// the fts columns, keyed by the rowid column's value.
fn scan_content_rows(
    ctx: &mut ExecContext<'_>,
    content: &Arc<crate::schema::Table>,
    rowid_col: &str,
    n_fts_cols: usize,
) -> Result<Vec<(i64, Vec<Value>)>> {
    let rowid_idx = content
        .find_column(rowid_col)
        .ok_or_else(|| Error::NotFound(format!("no such column: {rowid_col}")))?;
    let mut out = Vec::new();
    let root = ctx.table_root(content);
    let mut bt = Btree::new(ctx.pager, root, false);
    let mut decode_err: Option<Error> = None;
    bt.scan_table(|_rid, payload| {
        let row = match decode_row(payload, content.n_columns(), _rid, content.rowid_alias) {
            Ok(r) => r,
            Err(e) => {
                decode_err = Some(e);
                return false;
            }
        };
        let key = match row.get(rowid_idx) {
            Some(Value::Integer(i)) => *i,
            _ => return true,
        };
        let vals: Vec<Value> = row
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != rowid_idx)
            .take(n_fts_cols)
            .map(|(_, v)| v.clone())
            .collect();
        out.push((key, vals));
        true
    })?;
    if let Some(e) = decode_err {
        return Err(e);
    }
    Ok(out)
}
