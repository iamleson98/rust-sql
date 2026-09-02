//! Trigger execution: fire DML triggers (BEFORE/AFTER INSERT/UPDATE/DELETE,
//! FOR EACH ROW, optional WHEN guard) with NEW/OLD row substitution.
//!
//! The trigger body's statements are re-planned per fired row with every
//! `NEW.col` / `OLD.col` reference substituted by the row's literal value —
//! the same "substitute-then-execute" strategy the subquery rewriter uses.
//! This makes bodies work across all statement shapes (INSERT VALUES,
//! INSERT..SELECT, UPDATE SET, DELETE WHERE) without new binding machinery.
//!
//! Recursion: like SQLite's default (recursive_triggers=off), a trigger
//! fired from inside another trigger's body does not re-fire triggers —
//! `trigger_depth` guards against runaway recursion with a clear error.

use crate::executor::{execute, ExecContext};
use crate::sql::ast::{Expr, Statement, TriggerEvent, TriggerWhen};
use crate::types::{Row, Value};
use std::sync::Arc;

/// Hard recursion cap (SQLite's default max trigger depth is 1000; ours is
/// lower because each level re-plans statements).
const MAX_TRIGGER_DEPTH: u32 = 64;

/// Fire the triggers for `event`/`phase` on `table`.
///
/// * `new_row` — the post-change row (INSERT/UPDATE), or None (DELETE).
/// * `old_row` — the pre-change row (UPDATE/DELETE), or None (INSERT).
/// * `col_names` — the table's column names, positional (for NEW.x/OLD.x).
pub(crate) fn fire_triggers(
    ctx: &mut ExecContext<'_>,
    table: &crate::schema::Table,
    event: &TriggerEvent,
    phase: TriggerWhen,
    new_row: Option<&Row>,
    old_row: Option<&Row>,
    col_names: &[String],
) -> crate::error::Result<()> {
    if ctx.trigger_depth >= MAX_TRIGGER_DEPTH {
        return Err(crate::error::Error::semantic(format!(
            "trigger recursion exceeded {} levels on table {}",
            MAX_TRIGGER_DEPTH, table.name
        )));
    }
    // SQLite's default (PRAGMA recursive_triggers = OFF): a statement
    // running inside another trigger's body does NOT fire triggers. This
    // is what keeps self-inserting AFTER INSERT triggers (audit logs,
    // derived rows) from exploding into infinite recursion.
    if ctx.trigger_depth > 0 && !ctx.pager.recursive_triggers_enabled() {
        return Ok(());
    }
    let triggers = ctx.catalog().triggers_on_table(&table.name);
    if triggers.is_empty() {
        return Ok(());
    }
    for trig in triggers {
        if trig.when != phase {
            continue;
        }
        // Event match: UPDATE OF (cols) fires only when the changed columns
        // intersect the declared list (empty list = any UPDATE).
        let matches_event = trig.events.iter().any(|e| match (e, event) {
            (TriggerEvent::Insert, TriggerEvent::Insert) => true,
            (TriggerEvent::Delete, TriggerEvent::Delete) => true,
            (TriggerEvent::Update(list), TriggerEvent::Update(changed)) => {
                list.is_empty()
                    || list
                        .iter()
                        .any(|c| changed.iter().any(|cc| c.eq_ignore_ascii_case(cc)))
            }
            _ => false,
        });
        if !matches_event {
            continue;
        }
        // WHEN guard: evaluate with NEW/OLD bound as a combined row.
        if let Some(w) = &trig.when_clause {
            let v = eval_with_new_old(w, new_row, old_row, col_names, ctx)?;
            if !v.is_truthy() {
                continue;
            }
        }
        // Execute the body with NEW/OLD substituted to literals.
        ctx.trigger_depth += 1;
        let result = (|| {
            for stmt in &trig.body {
                let mut s = stmt.clone();
                substitute_new_old(&mut s, new_row, old_row, col_names)?;
                let plan = match &s {
                    Statement::Insert(_) => crate::api::Database::plan_insert(ctx.catalog(), &s)?,
                    Statement::Update(_) => crate::api::Database::plan_update(ctx.catalog(), &s)?,
                    Statement::Delete(_) => crate::api::Database::plan_delete(ctx.catalog(), &s)?,
                    Statement::Select(sel) => {
                        let mut planner = crate::planner::Planner::new(ctx.catalog());
                        planner.plan_select(sel)?
                    }
                    _ => {
                        return Err(crate::error::Error::semantic(format!(
                            "unsupported statement in trigger {} body",
                            trig.name
                        )));
                    }
                };
                let mut plan = plan;
                if crate::executor::plan_has_subqueries(&plan) {
                    plan = crate::executor::rewrite_plan_subqueries(&plan, ctx)?;
                }
                execute(&plan, ctx)?;
            }
            Ok(())
        })();
        ctx.trigger_depth -= 1;
        result?;
    }
    Ok(())
}

/// Evaluate an expression with NEW/OLD row references bound: the evaluation
/// row is `[new_row..., old_row...]` with column names `["new.c"...,
/// "old.c"...]`, so `NEW.c` resolves by qualified lookup.
fn eval_with_new_old(
    expr: &Expr,
    new_row: Option<&Row>,
    old_row: Option<&Row>,
    col_names: &[String],
    ctx: &ExecContext<'_>,
) -> crate::error::Result<Value> {
    let mut combined: Row = Vec::new();
    let mut names: Vec<String> = Vec::new();
    if let Some(n) = new_row {
        combined.extend(n.iter().cloned());
        names.extend(col_names.iter().map(|c| format!("new.{}", c)));
    }
    if let Some(o) = old_row {
        combined.extend(o.iter().cloned());
        names.extend(col_names.iter().map(|c| format!("old.{}", c)));
    }
    crate::executor::eval_row_public(expr, &combined, &names, &ctx.params, &ctx.named_params)
}

/// True when the table has at least one trigger for this event (lets hot
/// DML paths skip all trigger work with a single catalog lookup).
#[inline]
pub(crate) fn has_triggers_for(
    ctx: &ExecContext<'_>,
    table: &crate::schema::Table,
    event: &TriggerEvent,
) -> bool {
    let triggers = ctx.catalog().triggers_on_table(&table.name);
    triggers.iter().any(|t| {
        t.events.iter().any(|e| {
            matches!(
                (e, event),
                (TriggerEvent::Insert, TriggerEvent::Insert)
                    | (TriggerEvent::Delete, TriggerEvent::Delete)
                    | (TriggerEvent::Update(_), TriggerEvent::Update(_))
            )
        })
    })
}

/// Replace every `NEW.col` / `OLD.col` reference in a statement with the
/// literal value from the bound row. Unknown columns error (typo safety).
fn substitute_new_old(
    stmt: &mut Statement,
    new_row: Option<&Row>,
    old_row: Option<&Row>,
    col_names: &[String],
) -> crate::error::Result<()> {
    let lookup = |qual: &str, name: &str| -> Option<Value> {
        let row = match qual.to_ascii_lowercase().as_str() {
            "new" => new_row,
            "old" => old_row,
            _ => return None,
        }?;
        let idx = col_names
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))?;
        row.get(idx).cloned()
    };
    fn walk_expr(
        e: &mut Expr,
        lookup: &dyn Fn(&str, &str) -> Option<Value>,
    ) -> crate::error::Result<()> {
        match e {
            Expr::Column {
                table: Some(t),
                name,
            } => {
                if let Some(v) = lookup(t, name) {
                    *e = Expr::Literal(v);
                } else if t.eq_ignore_ascii_case("new") || t.eq_ignore_ascii_case("old") {
                    return Err(crate::error::Error::semantic(format!(
                        "no such column: {}.{} (NEW is only bound in INSERT/UPDATE triggers, OLD in UPDATE/DELETE)",
                        t, name
                    )));
                }
                Ok(())
            }
            Expr::Binary { left, right, .. } => {
                walk_expr(left, lookup)?;
                walk_expr(right, lookup)
            }
            Expr::Unary { expr, .. } => walk_expr(expr, lookup),
            Expr::Between {
                expr, low, high, ..
            } => {
                walk_expr(expr, lookup)?;
                walk_expr(low, lookup)?;
                walk_expr(high, lookup)
            }
            Expr::In { expr, source, .. } => {
                walk_expr(expr, lookup)?;
                if let crate::sql::ast::InSource::List(list) = source {
                    for item in list {
                        walk_expr(item, lookup)?;
                    }
                }
                Ok(())
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                walk_expr(expr, lookup)?;
                walk_expr(pattern, lookup)?;
                if let Some(es) = escape {
                    walk_expr(es, lookup)?;
                }
                Ok(())
            }
            Expr::IsNull { expr, .. } => walk_expr(expr, lookup),
            Expr::Is { left, right, .. } => {
                walk_expr(left, lookup)?;
                walk_expr(right, lookup)
            }
            Expr::Function { args, filter, .. } => {
                for a in args {
                    walk_expr(a, lookup)?;
                }
                if let Some(f) = filter {
                    walk_expr(f, lookup)?;
                }
                Ok(())
            }
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                if let Some(o) = operand {
                    walk_expr(o, lookup)?;
                }
                for (w, t) in whens {
                    walk_expr(w, lookup)?;
                    walk_expr(t, lookup)?;
                }
                if let Some(el) = else_ {
                    walk_expr(el, lookup)?;
                }
                Ok(())
            }
            Expr::Cast { expr, .. } => walk_expr(expr, lookup),
            Expr::Collate { expr, .. } => walk_expr(expr, lookup),
            Expr::Row(list) => {
                for item in list {
                    walk_expr(item, lookup)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    fn walk_body(
        body: &mut crate::sql::ast::SelectBody,
        lookup: &dyn Fn(&str, &str) -> Option<Value>,
    ) -> crate::error::Result<()> {
        match body {
            crate::sql::ast::SelectBody::Simple(s) => {
                for c in s.columns.iter_mut() {
                    if let crate::sql::ast::ResultColumn::Expr { expr, .. } = c {
                        walk_expr(expr, lookup)?;
                    }
                }
                if let Some(w) = s.where_clause.as_mut() {
                    walk_expr(w, lookup)?;
                }
                for g in s.group_by.iter_mut() {
                    walk_expr(g, lookup)?;
                }
                if let Some(h) = s.having.as_mut() {
                    walk_expr(h, lookup)?;
                }
                Ok(())
            }
            crate::sql::ast::SelectBody::Binary { left, right, .. } => {
                walk_body(left, lookup)?;
                walk_body(right, lookup)
            }
        }
    }

    fn walk_select(
        sel: &mut crate::sql::ast::SelectStatement,
        lookup: &dyn Fn(&str, &str) -> Option<Value>,
    ) -> crate::error::Result<()> {
        walk_body(&mut sel.body, lookup)?;
        for t in sel.order_by.iter_mut() {
            walk_expr(&mut t.expr, lookup)?;
        }
        Ok(())
    }
    match stmt {
        Statement::Insert(ins) => {
            match &mut ins.source {
                crate::sql::ast::InsertSource::Values(rows) => {
                    for row in rows {
                        for e in row {
                            walk_expr(e, &lookup)?;
                        }
                    }
                }
                crate::sql::ast::InsertSource::Select(sel) => {
                    walk_select(sel, &lookup)?;
                }
                _ => {}
            }
            if let Some(u) = ins.upsert.as_mut() {
                // ON CONFLICT ... DO UPDATE SET expr = expr [WHERE expr]
                if let crate::sql::ast::UpsertAction::DoUpdate { set, where_clause } = &mut u.action
                {
                    for (_, e) in set.iter_mut() {
                        walk_expr(e, &lookup)?;
                    }
                    if let Some(w) = where_clause.as_mut() {
                        walk_expr(w, &lookup)?;
                    }
                }
            }
            Ok(())
        }
        Statement::Update(upd) => {
            for (_, e) in upd.set.iter_mut() {
                walk_expr(e, &lookup)?;
            }
            if let Some(w) = upd.where_clause.as_mut() {
                walk_expr(w, &lookup)?;
            }
            Ok(())
        }
        Statement::Delete(del) => {
            if let Some(w) = del.where_clause.as_mut() {
                walk_expr(w, &lookup)?;
            }
            Ok(())
        }
        Statement::Select(sel) => walk_select(sel, &lookup),
        _ => Ok(()),
    }
}

/// Convenience: fire AFTER-INSERT triggers for a freshly inserted row.
#[allow(dead_code)]
pub(crate) fn after_insert(
    ctx: &mut ExecContext<'_>,
    table: &Arc<crate::schema::Table>,
    new_row: &Row,
) -> crate::error::Result<()> {
    fire_triggers(
        ctx,
        table,
        &TriggerEvent::Insert,
        TriggerWhen::After,
        Some(new_row),
        None,
        &table.col_names,
    )
}
