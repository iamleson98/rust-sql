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

/// One conjunct of a WHERE clause (an AND-chain element).
fn split_conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    if let Expr::Binary { op: BinaryOp::And, left, right } = e {
        split_conjuncts(left, out);
        split_conjuncts(right, out);
    } else {
        out.push(e);
    }
}

/// Rebuild an AND-chain from owned conjuncts.
fn rebuild_and(mut conjuncts: Vec<Expr>) -> Option<Expr> {
    let first = conjuncts.pop()?;
    Some(conjuncts.into_iter().rev().fold(first, |acc, e| Expr::Binary {
        op: BinaryOp::And,
        left: Box::new(e),
        right: Box::new(acc),
    }))
}

/// Extract vtab constraints from a predicate: conjuncts of the shape
/// `<vtab-column> <op> <expr>` (or mirrored). Returns the constraints and,
/// for each conjunct, whether it was consumed.
fn extract_constraints(
    predicate: &Expr,
    table: &Arc<crate::schema::Table>,
    alias: Option<&str>,
) -> (Vec<VtabConstraint>, Vec<bool>) {
    let mut conjuncts = Vec::new();
    split_conjuncts(predicate, &mut conjuncts);
    let prefix = alias.unwrap_or(&table.name);
    let mut constraints = Vec::new();
    let mut consumed = Vec::new();
    for c in conjuncts {
        let mut matched = false;
        if let Expr::Binary { op, left, right } = c {
            let op = *op;
            // Column-op-expr and expr-op-column (mirrored).
            if let (Some(col), Some(vop)) = (column_of(left, table, prefix), vtab_op(op)) {
                constraints.push(VtabConstraint {
                    column: col,
                    op: vop,
                    expr: (**right).clone(),
                });
                matched = true;
            } else if let (Some(col), Some(vop)) = (column_of(right, table, prefix), vtab_op(flip_op(op))) {
                constraints.push(VtabConstraint {
                    column: col,
                    op: vop,
                    expr: (**left).clone(),
                });
                matched = true;
            }
        } else if let Expr::Like { op, expr, pattern, negated: false, .. } = c {
            // `col LIKE pattern` (GLOB too)
            if let Some(col) = column_of(expr, table, prefix) {
                let vop = match op {
                    LikeOp::Like => VtabConstraintOp::Like,
                    LikeOp::Glob => VtabConstraintOp::Glob,
                    _ => VtabConstraintOp::Like,
                };
                constraints.push(VtabConstraint {
                    column: col,
                    op: vop,
                    expr: (**pattern).clone(),
                });
                matched = true;
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
fn column_of(e: &Expr, table: &Arc<crate::schema::Table>, prefix: &str) -> Option<Option<usize>> {
    if let Expr::Column { table: ref_t, name } = e {
        if let Some(t) = ref_t {
            if !t.eq_ignore_ascii_case(prefix) && !t.eq_ignore_ascii_case(&table.name) {
                return None;
            }
        }
        let lower = name.to_ascii_lowercase();
        if ["rowid", "oid", "_rowid_"].contains(&lower.as_str()) {
            return Some(None);
        }
        if let Some(idx) = table.find_column(name) {
            return Some(Some(idx));
        }
    }
    None
}

/// Column names for a vtab scan's output (mirrors exec_scan).
fn vtab_output_columns(table: &Arc<crate::schema::Table>, alias: Option<&str>) -> Arc<[String]> {
    let prefix = alias.unwrap_or(&table.name);
    if prefix == table.name {
        table.qualified_col_names.clone()
    } else {
        table
            .columns
            .iter()
            .map(|c| format!("{}.{}", prefix, c.name))
            .collect::<Vec<String>>()
            .into()
    }
}

/// Scan a virtual table, applying `predicate` (constraints pushed into the
/// module via best_index; the rest applied as a residual filter).
/// Returns (columns, (rowid, row) pairs).
pub(crate) fn scan_vtab(
    ctx: &mut ExecContext<'_>,
    table: &Arc<crate::schema::Table>,
    alias: Option<&str>,
    predicate: Option<&Expr>,
) -> Result<(Arc<[String]>, Vec<(i64, Row)>)> {
    let inst = table
        .vtab
        .as_ref()
        .ok_or_else(|| Error::corruption(format!("vtab exec on non-virtual table {}", table.name)))?;
    inst.ensure_connected()?;

    // 1. best_index + cursor open (state lock held only for these calls).
    struct Prepared {
        info: IndexInfo,
        filter_args: Vec<Value>,
    }
    let prepared: Prepared = inst.with_table(|vt| {
        let (constraints, _consumed) = match predicate {
            Some(p) => extract_constraints(p, table, alias),
            None => (Vec::new(), Vec::new()),
        };
        let mut info = vt.best_index(&constraints)?;
        if info.handled.len() != constraints.len() {
            // Defensive: modules must return one flag per constraint.
            info.handled.resize(constraints.len(), false);
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
        Ok(Prepared { info, filter_args })
    })?;

    // 2. Residual predicate: conjuncts the module did NOT handle. A
    // conjunct "handled" = extracted as a constraint AND marked handled by
    // best_index (info.handled). Conjuncts that never extracted are always
    // residual. Constraint j corresponds to the j-th MATCHED conjunct.
    let residual: Option<Expr> = match predicate {
        Some(p) => {
            let (_constraints, consumed) = extract_constraints(p, table, alias);
            let handled_flags = &prepared.info.handled;
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
                            ci += 1;
                            if handled { None } else { Some(e.clone()) }
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

    // 3. Drive the cursor.
    let n_cols = table.n_columns();
    let columns = vtab_output_columns(table, alias);
    let mut out: Vec<(i64, Row)> = Vec::new();
    let mut cursor = inst.with_table(|vt| vt.open())?;
    cursor.filter(prepared.info.idx_num, prepared.info.idx_str.as_deref(), &prepared.filter_args)?;
    while !cursor.eof() {
        let rowid = cursor.rowid()?;
        let mut row = Vec::with_capacity(n_cols);
        for i in 0..n_cols {
            row.push(cursor.column(i)?);
        }
        out.push((rowid, row));
        cursor.next()?;
    }
    drop(cursor);

    // 4. Residual filter.
    if let Some(pred) = &residual {
        let params: &[Value] = &ctx.params;
        let named_params = &ctx.named_params;
        let col_names: Vec<String> = columns.iter().cloned().collect();
        out.retain(|(_, row)| {
            match eval_row(pred, row, &col_names, params, named_params) {
                Ok(v) => v.is_truthy(),
                Err(_) => false,
            }
        });
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
    let inst = table
        .vtab
        .as_ref()
        .ok_or_else(|| Error::corruption("vtab exec on non-virtual table".to_string()))?;
    if !inst.writable()? {
return Err(Error::semantic(format!("cannot INSERT into read-only virtual table {}", table.name)));
    }
    let n_cols = table.n_columns();
    let mut changes = 0i64;
    let mut last_rowid = ctx.last_insert_rowid;
    for row in source_rows {
        let mut values: Vec<Option<Value>> = vec![None; n_cols];
        if let Some(idxs) = column_indices {
            if idxs.len() != row.len() {
                return Err(Error::semantic("table {} has a different number of columns".replace("{}", &table.name)));
            }
            for (i, col_idx) in idxs.iter().enumerate() {
                values[*col_idx] = Some(row[i].clone());
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
            for (i, v) in row.into_iter().enumerate() {
                values[i] = Some(v);
            }
        }
        // NOT NULL enforcement: vtab columns are nullable by declaration;
        // nothing further to check. Conflict handling: modules decide.
        let _ = on_conflict;
        let op = crate::plugin::vtab::UpdateOp {
            old_rowid: None,
            new_rowid: None,
            columns: values,
        };
        let assigned = inst.with_table(|vt| vt.update(vec![op]))?;
        changes += 1;
        if let Some(Some(rid)) = assigned.first().map(|r| *r) {
            last_rowid = rid;
        }
    }
    ctx.changes = changes;
    ctx.last_insert_rowid = last_rowid;
    Ok(())
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
return Err(Error::semantic(format!("cannot UPDATE read-only virtual table {}", table.name)));
    }
    let pairs = scan_vtab_for_dml(ctx, table, predicate)?;
    let n_cols = table.n_columns();
    let col_names: Vec<String> = table.columns.iter().map(|c| format!("{}.{}", table.name, c.name)).collect();
    let params: Vec<Value> = ctx.params.clone();
    let named = ctx.named_params.clone();
    let mut ops = Vec::with_capacity(pairs.len());
    for (rowid, row) in pairs {
        let mut columns: Vec<Option<Value>> = vec![None; n_cols];
        for (col_idx, expr) in assignments {
            let v = eval_row(expr, &row, &col_names, &params, &named)?;
            columns[*col_idx] = Some(v);
        }
        ops.push(crate::plugin::vtab::UpdateOp {
            old_rowid: Some(rowid),
            new_rowid: Some(rowid),
            columns,
        });
    }
    let n = ops.len() as i64;
    inst.with_table(|vt| vt.update(ops))?;
    ctx.changes = n;
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
return Err(Error::semantic(format!("cannot DELETE from read-only virtual table {}", table.name)));
    }
    let pairs = scan_vtab_for_dml(ctx, table, predicate)?;
    let ops: Vec<crate::plugin::vtab::UpdateOp> = pairs
        .into_iter()
        .map(|(rowid, _)| crate::plugin::vtab::UpdateOp {
            old_rowid: Some(rowid),
            new_rowid: None,
            columns: Vec::new(),
        })
        .collect();
    let n = match ops.len() {
        0 => 0,
        n => {
            inst.with_table(|vt| vt.update(ops))?;
            n as i64
        }
    };
    ctx.changes = n;
    Ok(())
}

/// EXPLAIN row rendering for a vtab scan.
#[allow(dead_code)]
pub(crate) fn explain_scan_vtab(table: &Arc<crate::schema::Table>) -> Vec<Row> {
    let module = table.vtab.as_ref().map(|v| v.module_name.clone()).unwrap_or_default();
    vec![
        vec![
            Value::Text("SCAN".into()),
            Value::Text(format!("{} VIRTUAL TABLE", table.name).into()),
            Value::Text(format!("module={}", module).into()),
        ],
    ]
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
