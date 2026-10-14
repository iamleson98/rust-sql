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
use crate::sql::ast::{ConflictResolution, Expr, Statement, TriggerEvent, TriggerWhen};
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
// First-fire name resolution for a trigger: the WHEN clause and every
// body statement are validated against a scope of the trigger TABLE's
// columns (bare) plus the NEW/OLD pseudo-tables (qualified-only, the
// table's columns). SQLite's fire-time contract — `no such column:
// NEW.x` / `no such column: x` — before any body statement executes.
fn validate_trigger_first_fire(
    catalog: &crate::schema::Catalog,
    trig: &crate::schema::Trigger,
    table: &crate::schema::Table,
) -> crate::error::Result<()> {
    use crate::planner::namecheck as nc;
    // Reuse the module's public entry on a synthetic scope: the body
    // statements are full Statements — validate each with NEW/OLD
    // exposed as ordinary (qualified) sources through a wrapper scope.
    // namecheck's public API validates standalone statements; the
    // NEW/OLD layer is provided via a one-shot wrapper that walks the
    // statement with the trigger scope as the outer level.
    let cols: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let new_old_as_sources = || -> nc::TriggerScope {
        nc::TriggerScope::new(&table.name, cols.clone(), !table.without_rowid)
    };
    let scope = new_old_as_sources();
    if let Some(w) = &trig.when_clause {
        nc::validate_expr_in_scope(catalog, w, &scope)?;
    }
    for stmt in &trig.body {
        nc::validate_stmt_in_scope(catalog, stmt, &scope)?;
    }
    Ok(())
}

/// A trigger's WHEN clause and body with SQLite's double-quoted-string
/// fallback applied: each `"name"` that resolves to no column (NEW/OLD
/// and the trigger table in scope) is the string 'name' — the CREATE
/// TRIGGER text re-parses with it as a literal. Anything else that fails
/// to resolve is kept as written (it errors at first fire, as in SQLite).
pub(crate) fn trigger_dqs(
    catalog: &crate::schema::Catalog,
    create_sql: &str,
    when_clause: Option<crate::sql::ast::Expr>,
    body: Vec<crate::sql::ast::Statement>,
) -> (
    Option<crate::sql::ast::Expr>,
    Vec<crate::sql::ast::Statement>,
) {
    use crate::sql::ast::{CreateStatement, Statement};
    let (mut when_clause, mut body) = (when_clause, body);
    let mut literals: Vec<String> = Vec::new();
    loop {
        let Ok((Statement::Create(CreateStatement::Trigger(ct)), seen)) =
            crate::sql::parser::parse_dqs(create_sql, &literals)
        else {
            return (when_clause, body);
        };
        if seen.is_empty() {
            return (when_clause, body);
        }
        let Some(table) = catalog.get_table(&ct.table) else {
            return (when_clause, body);
        };
        let probe = crate::schema::Trigger {
            name: ct.name.clone(),
            table: ct.table.clone(),
            when: ct.when,
            events: ct.events.clone(),
            for_each_row: ct.for_each_row,
            when_clause: when_clause.clone(),
            body: body.clone(),
            create_sql: create_sql.to_string(),
            bound_schema: None,
            dormant: false,
            has_foreign_refs: false,
            validated: std::sync::atomic::AtomicBool::new(false),
        };
        let name = match validate_trigger_first_fire(catalog, &probe, &table) {
            Err(crate::error::Error::NotFound(m)) => {
                m.strip_prefix("no such column: ").map(str::to_string)
            }
            _ => None,
        };
        let Some(name) = name else {
            return (when_clause, body);
        };
        if !seen.contains(&name) || literals.contains(&name) || literals.len() >= 64 {
            return (when_clause, body);
        }
        literals.push(name);
        match crate::sql::parser::parse_dqs(create_sql, &literals) {
            Ok((Statement::Create(CreateStatement::Trigger(t2)), _)) => {
                when_clause = t2.when_clause;
                body = t2.body;
            }
            _ => return (when_clause, body),
        }
    }
}

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
    // Any trigger body can execute arbitrary DML (including against
    // tables the OUTER statement is inserting into), so the insert
    // loops' per-row max-rowid overlay consults must become LIVE from
    // this point on (see ExecContext::triggers_fired_epoch).
    ctx.triggers_fired_epoch += 1;
    // SQLite's default (PRAGMA recursive_triggers = OFF): a trigger does
    // not fire ITSELF, directly or indirectly — but DIFFERENT triggers
    // chained at depth 2+ still run (SQLite's name-on-stack semantics;
    // pinned by the preupdate differential suite). The per-trigger
    // check lives in the loop below (trigger_stack).
    let triggers = ctx.catalog().trigger_list(&table.name);
    if triggers.is_empty() {
        return Ok(());
    }
    // SQLite fires same-event triggers in REVERSE creation order: its
    // trigger chain PREPENDS each new trigger and firing walks the chain
    // head-first (verified: two AFTER DELETE triggers A then B insert
    // rows B-first; three triggers C,B,A). Our list is creation-ordered,
    // so walk it backwards (deep-sweep: the delete-all with two
    // self-inserting audit triggers produced phase-shifted rows).
    for trig in triggers.iter().rev() {
        if trig.when != phase || !trig.fires_locally() {
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
        // First-fire validation (SQLite's lazy trigger contract: bodies
        // and WHEN clauses resolve at FIRE time, not CREATE time). The
        // NEW/OLD substitution below rewrites bound refs to literals; a
        // NEW/OLD ref that is UNKNOWN here (not a column of the table)
        // must error with SQLite's exact text before any body statement
        // runs. Validated once per Trigger registration (the Arc is
        // replaced by DDL, so the flag can never go stale).
        //
        // CROSS-DATABASE TEMP triggers (foreign refs in WHEN/body):
        // validate by PLANNING every body statement — with the foreign
        // channel armed, or on the owning attached engine — instead of
        // against the local catalog (which cannot see aux tables). The
        // channel is armed first (the WHEN guard may read aux too).
        let armed_channel = if trig.has_foreign_refs {
            crate::attach_fire::arm_foreign_channel(ctx, trig)?
        } else {
            false
        };
        if !trig.validated.load(std::sync::atomic::Ordering::Acquire) {
            if trig.has_foreign_refs {
                let check = || -> crate::error::Result<()> {
                    for s0 in &trig.body {
                        let mut s = s0.clone();
                        substitute_new_old(&mut s, new_row, old_row, col_names)?;
                        crate::attach_fire::validate_foreign_body(ctx, &s)?;
                    }
                    Ok(())
                };
                if let Err(e) = check() {
                    if armed_channel {
                        ctx.foreign = None;
                    }
                    return Err(e);
                }
            } else {
                validate_trigger_first_fire(ctx.catalog(), trig, table)?;
            }
            trig.validated
                .store(true, std::sync::atomic::Ordering::Release);
        }
        // WHEN guard: evaluate with NEW/OLD bound as a combined row
        // (with the channel armed — the guard may read attached tables).
        if let Some(w) = &trig.when_clause {
            let fires = eval_with_new_old(w, new_row, old_row, col_names, ctx)?;
            if !fires {
                if armed_channel {
                    ctx.foreign = None;
                }
                continue;
            }
        }
        // Recursive-trigger gate: with recursive_triggers OFF (SQLite's
        // default), a trigger already on the execution stack does not
        // re-fire (directly or through a chain) — but OTHER triggers at
        // this depth still run.
        if ctx
            .trigger_stack
            .iter()
            .any(|n| n.eq_ignore_ascii_case(&trig.name))
            && !ctx.pager.recursive_triggers_enabled()
        {
            if armed_channel {
                ctx.foreign = None;
            }
            continue;
        }
        // Execute the body with NEW/OLD substituted to literals.
        // last_insert_rowid() inside the body sees the body's own inserts;
        // once the trigger program ends it reverts (sqlite3_last_insert_rowid).
        let saved_last_insert_rowid = ctx.last_insert_rowid;
        // The body's rows count toward total_changes() only.
        let saved_changes = ctx.changes;
        ctx.trigger_depth += 1;
        ctx.trigger_stack.push(trig.name.clone());
        // Preupdate depth: rows written by the trigger body are one
        // nesting level deeper than the statement's own rows (SQLite:
        // top-level triggers fire their rows at depth 1, nested at 2).
        let _preupdate_depth = crate::preupdate::enter_nested_change();
        // SQLite's codeTriggerProgram: the firing statement's ON CONFLICT
        // policy, when it has one, REPLACES each step's own (`INSERT OR
        // REPLACE` firing a body `INSERT` replaces where the body alone
        // would abort); a DELETE step fires its own programs with the
        // default policy.
        let firing_or = ctx.firing_or;
        let result = (|| {
            for (step_idx, stmt) in trig.body.iter().enumerate() {
                // Compiled program: plan the step ONCE per statement with
                // NEW / OLD as positional parameters, then bind this row.
                if !trig.has_foreign_refs {
                    let key = (
                        Arc::as_ptr(trig) as usize,
                        step_idx,
                        conflict_code(firing_or),
                        new_row.is_some(),
                        old_row.is_some(),
                    );
                    let compiled = match ctx.trigger_plans.get(&key) {
                        Some(c) => c.clone(),
                        None => {
                            let c = compile_trigger_step(
                                ctx,
                                stmt,
                                firing_or,
                                new_row.is_some(),
                                old_row.is_some(),
                                col_names,
                            )?;
                            ctx.trigger_plans.insert(key, c.clone());
                            c
                        }
                    };
                    if let Some((plan, step_or, has_subqueries)) = compiled {
                        let n = col_names.len();
                        let width = if old_row.is_some() { 2 * n } else { n };
                        let mut vals = Vec::with_capacity(width);
                        for i in 0..n {
                            vals.push(
                                new_row
                                    .and_then(|r| r.get(i).cloned())
                                    .unwrap_or(Value::Null),
                            );
                        }
                        if let Some(r) = old_row {
                            for i in 0..n {
                                vals.push(r.get(i).cloned().unwrap_or(Value::Null));
                            }
                        }
                        let saved_params = std::mem::replace(&mut ctx.params, vals);
                        let saved_named = std::mem::take(&mut ctx.named_params);
                        let rewritten = if has_subqueries {
                            crate::executor::rewrite_plan_subqueries(&plan, ctx).map(Some)
                        } else {
                            Ok(None)
                        };
                        let step = match rewritten {
                            Ok(r) => {
                                let saved_or = std::mem::replace(&mut ctx.firing_or, step_or);
                                let step = execute(r.as_ref().unwrap_or(&plan), ctx);
                                ctx.firing_or = saved_or;
                                step.map(|_| ())
                            }
                            Err(e) => Err(e),
                        };
                        ctx.params = saved_params;
                        ctx.named_params = saved_named;
                        step?;
                        continue;
                    }
                }
                let mut s = stmt.clone();
                let step_or = match &mut s {
                    Statement::Insert(i) => {
                        if firing_or.is_some() {
                            i.or = firing_or;
                        }
                        i.or
                    }
                    Statement::Update(u) => {
                        if firing_or.is_some() {
                            u.or = firing_or;
                        }
                        u.or
                    }
                    _ => None,
                };
                substitute_new_old(&mut s, new_row, old_row, col_names)?;
                if trig.has_foreign_refs && crate::attach_fire::try_foreign_body(ctx, &s)? {
                    continue;
                }
                let plan = if trig.has_foreign_refs {
                    crate::api::Database::plan_body_with_channel(
                        ctx.catalog(),
                        &s,
                        ctx.foreign.clone(),
                    )?
                } else {
                    match &s {
                        Statement::Insert(_) => {
                            crate::api::Database::plan_insert(ctx.catalog(), &s)?
                        }
                        Statement::Update(_) => {
                            crate::api::Database::plan_update(ctx.catalog(), &s)?
                        }
                        Statement::Delete(_) => {
                            crate::api::Database::plan_delete(ctx.catalog(), &s)?
                        }
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
                    }
                };
                let mut plan = plan;
                if crate::executor::plan_has_subqueries(&plan) {
                    plan = crate::executor::rewrite_plan_subqueries(&plan, ctx)?;
                }
                let saved_or = std::mem::replace(&mut ctx.firing_or, step_or);
                let step = execute(&plan, ctx);
                ctx.firing_or = saved_or;
                step?;
            }
            Ok(())
        })();
        if armed_channel {
            ctx.foreign = None;
        }
        ctx.trigger_depth -= 1;
        ctx.trigger_stack.pop();
        ctx.last_insert_rowid = saved_last_insert_rowid;
        crate::executor::change_counters::note_conn_rowid(saved_last_insert_rowid);
        ctx.trigger_changes += ctx.changes - saved_changes;
        ctx.changes = saved_changes;
        result?;
    }
    Ok(())
}

/// Evaluate a trigger's WHEN guard with NEW/OLD row references bound:
/// the evaluation row is `[new_row..., old_row...]` with column names
/// `["new.c"..., "old.c"...]`, so `NEW.c` resolves by qualified lookup.
/// The guard is a condition (sqlite3ExprIfFalse, SQLITE_JUMPIFNULL):
/// true when the trigger fires.
pub(crate) fn eval_with_new_old_pub(
    expr: &Expr,
    new_row: Option<&Row>,
    old_row: Option<&Row>,
    col_names: &[String],
    ctx: &ExecContext<'_>,
) -> crate::error::Result<bool> {
    eval_with_new_old(expr, new_row, old_row, col_names, ctx)
}

fn eval_with_new_old(
    expr: &Expr,
    new_row: Option<&Row>,
    old_row: Option<&Row>,
    col_names: &[String],
    ctx: &ExecContext<'_>,
) -> crate::error::Result<bool> {
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
    crate::executor::eval_row_where(expr, &combined, &names, &ctx.params, &ctx.named_params)
}

/// True when the table has at least one trigger for this event (lets hot
/// DML paths skip all trigger work with a single catalog lookup).
#[inline]
pub(crate) fn has_triggers_for(
    ctx: &ExecContext<'_>,
    table: &crate::schema::Table,
    event: &TriggerEvent,
) -> bool {
    let triggers = ctx.catalog().trigger_list(&table.name);
    triggers.iter().any(|t| {
        t.fires_locally()
            && t.events.iter().any(|e| {
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
pub(crate) fn substitute_new_old_pub(
    stmt: &mut Statement,
    new_row: Option<&Row>,
    old_row: Option<&Row>,
    col_names: &[String],
) -> crate::error::Result<()> {
    substitute_new_old(stmt, new_row, old_row, col_names)
}

fn substitute_new_old(
    stmt: &mut Statement,
    new_row: Option<&Row>,
    old_row: Option<&Row>,
    col_names: &[String],
) -> crate::error::Result<()> {
    let lookup = |qual: &str, name: &str| -> Option<Expr> {
        let row = match qual.to_ascii_lowercase().as_str() {
            "new" => new_row,
            "old" => old_row,
            _ => return None,
        }?;
        let idx = col_names
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))?;
        row.get(idx).cloned().map(Expr::Literal)
    };
    rewrite_new_old(stmt, &lookup)
}

/// Rewrite every `NEW.col` / `OLD.col` reference into positional
/// parameter `?i` (NEW column i) / `?(n + i)` (OLD column i, n = the
/// table's column count) — the per-statement compiled trigger program's
/// registers (see `ExecContext::trigger_plans`). Same coverage and the
/// same unknown-column error as the literal substitution.
fn parametrize_new_old(
    stmt: &mut Statement,
    has_new: bool,
    has_old: bool,
    col_names: &[String],
) -> crate::error::Result<()> {
    let n = col_names.len();
    let lookup = |qual: &str, name: &str| -> Option<Expr> {
        let base = match qual.to_ascii_lowercase().as_str() {
            "new" if has_new => 0,
            "old" if has_old => n,
            _ => return None,
        };
        let idx = col_names
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))?;
        Some(Expr::Parameter((base + idx).to_string()))
    };
    rewrite_new_old(stmt, &lookup)
}

/// The NEW / OLD reference walker behind both rewrites: `lookup` yields
/// the replacement for a bound reference, `None` for an unbound one
/// (SQLite's "no such column" when it names NEW / OLD).
fn rewrite_new_old(
    stmt: &mut Statement,
    lookup: &dyn Fn(&str, &str) -> Option<Expr>,
) -> crate::error::Result<()> {
    fn walk_expr(
        e: &mut Expr,
        lookup: &dyn Fn(&str, &str) -> Option<Expr>,
    ) -> crate::error::Result<()> {
        match e {
            Expr::Column {
                table: Some(t),
                name,
            } => {
                if let Some(v) = lookup(t, name) {
                    *e = v;
                } else if t.eq_ignore_ascii_case("new") || t.eq_ignore_ascii_case("old") {
                    // SQLite-exact text (the same error covers both an
                    // unknown column AND the not-bound-here case).
                    return Err(crate::error::Error::NotFound(format!(
                        "no such column: {}.{}",
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
                    for item in std::sync::Arc::make_mut(list).iter_mut() {
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
        lookup: &dyn Fn(&str, &str) -> Option<Expr>,
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
        lookup: &dyn Fn(&str, &str) -> Option<Expr>,
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
                            walk_expr(e, lookup)?;
                        }
                    }
                }
                crate::sql::ast::InsertSource::Select(sel) => {
                    walk_select(sel, lookup)?;
                }
                _ => {}
            }
            let mut next = ins.upsert.as_mut();
            while let Some(u) = next {
                // ON CONFLICT ... DO UPDATE SET expr = expr [WHERE expr]
                if let crate::sql::ast::UpsertAction::DoUpdate { set, where_clause } = &mut u.action
                {
                    for (_, e) in set.iter_mut() {
                        walk_expr(e, lookup)?;
                    }
                    if let Some(w) = where_clause.as_mut() {
                        walk_expr(w, lookup)?;
                    }
                }
                next = u.next.as_deref_mut();
            }
            Ok(())
        }
        Statement::Update(upd) => {
            for (_, e) in upd.set.iter_mut() {
                walk_expr(e, lookup)?;
            }
            if let Some(w) = upd.where_clause.as_mut() {
                walk_expr(w, lookup)?;
            }
            Ok(())
        }
        Statement::Delete(del) => {
            if let Some(w) = del.where_clause.as_mut() {
                walk_expr(w, lookup)?;
            }
            Ok(())
        }
        Statement::Select(sel) => walk_select(sel, lookup),
        _ => Ok(()),
    }
}

/// A firing policy as a cache-key byte.
fn conflict_code(c: Option<ConflictResolution>) -> u8 {
    match c {
        None => 0,
        Some(ConflictResolution::Rollback) => 1,
        Some(ConflictResolution::Abort) => 2,
        Some(ConflictResolution::Fail) => 3,
        Some(ConflictResolution::Ignore) => 4,
        Some(ConflictResolution::Replace) => 5,
    }
}

/// Compile one trigger body step for the per-statement program cache:
/// the step with the firing policy applied and NEW / OLD rewritten to
/// positional parameters, planned once. `None` keeps the step on the
/// per-row literal-substitution path (a body that holds parameters of
/// its own — SQLite rejects those at CREATE TRIGGER — or a statement
/// kind the cache does not plan).
#[allow(clippy::type_complexity)]
fn compile_trigger_step(
    ctx: &ExecContext<'_>,
    stmt: &Statement,
    firing_or: Option<ConflictResolution>,
    has_new: bool,
    has_old: bool,
    col_names: &[String],
) -> crate::error::Result<
    Option<(
        Arc<crate::planner::plan::Plan>,
        Option<ConflictResolution>,
        bool,
    )>,
> {
    let (mut positional, mut named) = (0usize, Vec::new());
    crate::statement::collect_parameters(stmt, &mut positional, &mut named);
    if positional > 0 || !named.is_empty() {
        return Ok(None);
    }
    let mut s = stmt.clone();
    let step_or = match &mut s {
        Statement::Insert(i) => {
            if firing_or.is_some() {
                i.or = firing_or;
            }
            i.or
        }
        Statement::Update(u) => {
            if firing_or.is_some() {
                u.or = firing_or;
            }
            u.or
        }
        _ => None,
    };
    parametrize_new_old(&mut s, has_new, has_old, col_names)?;
    let plan = match &s {
        Statement::Insert(_) => crate::api::Database::plan_insert(ctx.catalog(), &s)?,
        Statement::Update(_) => crate::api::Database::plan_update(ctx.catalog(), &s)?,
        Statement::Delete(_) => crate::api::Database::plan_delete(ctx.catalog(), &s)?,
        Statement::Select(sel) => {
            let mut planner = crate::planner::Planner::new(ctx.catalog());
            planner.plan_select(sel)?
        }
        _ => return Ok(None),
    };
    let has_subqueries = crate::executor::plan_has_subqueries(&plan);
    Ok(Some((Arc::new(plan), step_or, has_subqueries)))
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
