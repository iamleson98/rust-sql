//! Cross-database trigger firing — the machinery behind SQLite's TEMP
//! triggers that target (or reference) ATTACHED databases.
//!
//! SQLite's rules (pinned against the 3.53.4 oracle; see
//! tests/attach_triggers.rs):
//!
//! - A TEMP trigger may target a table in ANY database. One bound to an
//!   ATTACHED table (`CREATE TEMP TRIGGER trg AFTER INSERT ON aux.t`)
//!   lives in the connection's temp schema and fires, per row, for THIS
//!   connection's writes to that table — INSERT, UPDATE and DELETE,
//!   with NEW/OLD visible and `RAISE` aborting the whole statement.
//! - Its body may read and write any database: qualified DML targets
//!   are allowed in TEMP trigger bodies (the one place SQLite permits
//!   them), and bare names bind through the connection search order.
//! - `DETACH` is allowed while such triggers exist; the trigger stays
//!   listed in the temp schema but never fires again — not even under a
//!   later re-ATTACH (the `dormant` flag).
//!
//! Implementation shape:
//!
//! - **The routed-DML driver** ([`routed_dml_with_triggers`]): when a
//!   DML statement targets an attached table that carries bound TEMP
//!   triggers, the statement never runs whole on the attached engine —
//!   the parent drives it row by row: candidates come from a SELECT on
//!   the target engine (mixed statements get the parent-side tables
//!   injected through the reverse channel), NEW rows are computed at
//!   the parent (SET expressions against the OLD row, defaults for
//!   omitted INSERT columns), and each row's write is a parameterized
//!   localized statement on the target engine so its own constraints,
//!   indexes, FKs and triggers fire natively. The parent's TEMP
//!   triggers wrap each write (BEFORE → write → AFTER), with WHEN
//!   guards and bodies running through the parent's FULL statement
//!   machinery — bodies may read/write any database and recurse.
//! - **The nested foreign body bridge** ([`try_foreign_body`]): a TEMP
//!   trigger on a LOCAL table whose body references attached databases
//!   fires through the ordinary executor fire path; body statements
//!   that ROUTE whole to one attached engine are executed through this
//!   bridge (the thread-db pointer the vtab xConnect bridge uses).
//!   Fully-local bodies keep the in-context fast path (they share the
//!   statement journal and root overlays); mixed body statements are
//!   rejected with a clear pointer to the TEMP-trigger-on-aux form.
//! - **Atomicity**: the driver spans a SAVEPOINT on every participating
//!   engine (and the parent) for the statement's duration — a failure
//!   anywhere rolls the statement back on every engine, SQLite's
//!   multi-database statement journal semantics. The local-table form
//!   spans savepoints on the attached engines from the dispatcher.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::api::Database;
use crate::error::{Error, Result};
use crate::sql::ast::*;
use crate::types::{Row, Value};

/// Driver recursion cap: a TEMP trigger's body writing another bound
/// table re-enters the driver (that table's triggers fire too). SQLite
/// caps trigger nesting far higher, but each level here re-drives a
/// statement; 16 nested cross-database trigger levels is already deep
/// in pathological territory.
const MAX_DRIVER_DEPTH: usize = 16;

thread_local! {
    static DRIVER_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// The synthetic SAVEPOINT name spanning one cross-database statement.
const SP_NAME: &str = "rsql_xdb_stmt";

/// True when the attached table carries live (non-dormant) bound TEMP
/// triggers — the routing sites' gate: no triggers, zero driver cost
/// (the statement routes whole as before).
pub(crate) fn has_bound_triggers(db: &Database, schema_lc: &str, table: &str) -> bool {
    !db.catalog
        .temp_triggers_bound_to(schema_lc, table)
        .is_empty()
}

/// Open the cross-statement scope: a SAVEPOINT on every attached
/// engine. Engines not in a transaction start one implicitly (the
/// engine's SAVEPOINT semantics mirror SQLite's). Returns the snapshot
/// of engines the caller must RELEASE (or roll back) at statement end.
fn open_aux_scope(db: &Database) -> Vec<Arc<RwLock<Database>>> {
    let engines: Vec<Arc<RwLock<Database>>> =
        db.attached_snapshot().into_iter().map(|(_, e)| e).collect();
    for engine in &engines {
        let mut aux = engine.write();
        let _ = aux.execute(&format!("SAVEPOINT \"{}\"", SP_NAME), []);
    }
    engines
}

/// Finish the scope: RELEASE (commit) or ROLLBACK TO + RELEASE (undo
/// the statement) on every engine the scope opened.
fn close_aux_scope(engines: &[Arc<RwLock<Database>>], rollback: bool) {
    for engine in engines {
        let mut aux = engine.write();
        if rollback {
            let _ = aux.execute(&format!("ROLLBACK TO \"{}\"", SP_NAME), []);
        }
        let _ = aux.execute(&format!("RELEASE \"{}\"", SP_NAME), []);
    }
}

/// A synthetic SQL prefix for the cached-statement machinery's DDL
/// classification (is_ddl_sql scans the first keyword only).
fn sql_hint(stmt: &Statement) -> String {
    let kw = match stmt {
        Statement::Insert(_) => "INSERT",
        Statement::Update(_) => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::Select(_) => "SELECT",
        Statement::Create(_) => "CREATE",
        Statement::Drop(_) => "DROP",
        Statement::Alter(_) => "ALTER",
        _ => "SELECT",
    };
    format!("{} /* cross-db trigger */", kw)
}

/// One bound trigger's event match against the DML event (the
/// executor's fire-path rule: UPDATE OF fires only when the changed
/// columns intersect the declared list; an empty list matches any
/// UPDATE).
fn event_matches(trig: &crate::schema::Trigger, event: &TriggerEvent) -> bool {
    trig.events.iter().any(|e| match (e, event) {
        (TriggerEvent::Insert, TriggerEvent::Insert) => true,
        (TriggerEvent::Delete, TriggerEvent::Delete) => true,
        (TriggerEvent::Update(list), TriggerEvent::Update(changed)) => {
            list.is_empty()
                || list
                    .iter()
                    .any(|c| changed.iter().any(|cc| c.eq_ignore_ascii_case(cc)))
        }
        _ => false,
    })
}

/// Evaluate one expression row-wise through the standard machinery (a
/// one-row VALUES select executed on the parent with the foreign
/// channel armed) — used for SET expressions, DEFAULT values and the
/// INSERT source arms, keeping every expression feature (subqueries,
/// functions, collations) on the same code path as a plain SELECT.
fn eval_exprs_at_parent(db: &Database, exprs: &[Expr], params: &[Value]) -> Result<Vec<Value>> {
    let sel = SelectStatement {
        with: None,
        body: SelectBody::Simple(SimpleSelect::values_row(exprs.to_vec())),
        order_by: Vec::new(),
        limit: None,
        offset: None,
    };
    let rows = db.exec_foreign_select(&Statement::Select(sel), params)?;
    Ok(rows.into_iter().next().unwrap_or_default())
}

/// Map INSERT-provided values onto the aux table's full column list,
/// applying the aux table's DEFAULT expressions for omitted columns
/// (SQLite builds the row — defaults applied — before BEFORE triggers
/// fire).
fn map_insert_row(
    db: &Database,
    aux_table: &crate::schema::Table,
    insert_cols: &[String],
    provided: &[Value],
    params: &[Value],
) -> Result<Row> {
    let mut row: Row = Vec::with_capacity(aux_table.columns.len());
    let mut provided_map: HashMap<String, &Value> = HashMap::new();
    for (c, v) in insert_cols.iter().zip(provided.iter()) {
        provided_map.insert(c.to_ascii_lowercase(), v);
    }
    for col in aux_table.columns.iter() {
        let hit = provided_map.get(&col.name.to_ascii_lowercase()).copied();
        match hit {
            Some(v) => row.push(v.clone()),
            None => match &col.default {
                Some(dexpr) => {
                    let v = eval_exprs_at_parent(db, std::slice::from_ref(dexpr), params)?;
                    row.push(v.into_iter().next().unwrap_or(Value::Null));
                }
                None => row.push(Value::Null),
            },
        }
    }
    Ok(row)
}

/// Compute the UPDATE's NEW row from an OLD row: SET expressions
/// evaluated against the OLD row's values (unqualified and
/// table/alias-qualified refs both resolve — the values-row select
/// below names its outputs positionally, so rewrite column refs in the
/// SET expressions to the row's positional parameters first).
fn compute_update_row(
    db: &Database,
    upd: &UpdateStatement,
    aux_table: &crate::schema::Table,
    old_row: &[Value],
    params: &[Value],
) -> Result<Row> {
    // Substitute column refs in each SET expression with the OLD row's
    // literal value (the trigger machinery's substitute strategy: refs
    // matching the target table/alias become literals; anything else —
    // subqueries, params — survives untouched).
    let qual = upd
        .alias
        .clone()
        .unwrap_or_else(|| upd.table.clone())
        .to_ascii_lowercase();
    let lookup = |t: Option<&str>, name: &str| -> Option<Value> {
        if let Some(t) = t {
            if !t.eq_ignore_ascii_case(&qual) {
                return None;
            }
        }
        let idx = aux_table
            .col_names
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))?;
        old_row.get(idx).cloned()
    };
    let mut new_row: Row = old_row.to_vec();
    for (col, expr) in upd.set.iter() {
        let mut e = expr.clone();
        substitute_row_refs(&mut e, &lookup)?;
        let idx = aux_table
            .col_names
            .iter()
            .position(|c| c.eq_ignore_ascii_case(col))
            .ok_or_else(|| Error::NotFound(format!("no such column: {}", col)))?;
        let v = eval_exprs_at_parent(db, std::slice::from_ref(&e), params)?;
        new_row[idx] = v.into_iter().next().unwrap_or(Value::Null);
    }
    Ok(new_row)
}

/// Replace `Expr::Column` refs that `lookup` resolves with literals —
/// the same walker discipline as the trigger machinery's
/// NEW/OLD substitution.
fn substitute_row_refs(
    e: &mut Expr,
    lookup: &dyn Fn(Option<&str>, &str) -> Option<Value>,
) -> Result<()> {
    match e {
        Expr::Column { table, name } => {
            if let Some(v) = lookup(table.as_deref(), name) {
                *e = Expr::Literal(v);
            }
            Ok(())
        }
        Expr::Binary { left, right, .. } => {
            substitute_row_refs(left, lookup)?;
            substitute_row_refs(right, lookup)
        }
        Expr::Unary { expr, .. } => substitute_row_refs(expr, lookup),
        Expr::Between {
            expr, low, high, ..
        } => {
            substitute_row_refs(expr, lookup)?;
            substitute_row_refs(low, lookup)?;
            substitute_row_refs(high, lookup)
        }
        Expr::In { expr, source, .. } => {
            substitute_row_refs(expr, lookup)?;
            if let InSource::List(list) = source {
                for item in Arc::make_mut(list).iter_mut() {
                    substitute_row_refs(item, lookup)?;
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
            substitute_row_refs(expr, lookup)?;
            substitute_row_refs(pattern, lookup)?;
            if let Some(es) = escape {
                substitute_row_refs(es, lookup)?;
            }
            Ok(())
        }
        Expr::IsNull { expr, .. } => substitute_row_refs(expr, lookup),
        Expr::Is { left, right, .. } => {
            substitute_row_refs(left, lookup)?;
            substitute_row_refs(right, lookup)
        }
        Expr::Function { args, filter, .. } => {
            for a in args {
                substitute_row_refs(a, lookup)?;
            }
            if let Some(f) = filter {
                substitute_row_refs(f, lookup)?;
            }
            Ok(())
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                substitute_row_refs(o, lookup)?;
            }
            for (w, t) in whens {
                substitute_row_refs(w, lookup)?;
                substitute_row_refs(t, lookup)?;
            }
            if let Some(el) = else_ {
                substitute_row_refs(el, lookup)?;
            }
            Ok(())
        }
        Expr::Cast { expr, .. } => substitute_row_refs(expr, lookup),
        Expr::Collate { expr, .. } => substitute_row_refs(expr, lookup),
        Expr::Row(list) => {
            for item in list {
                substitute_row_refs(item, lookup)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The per-row WHERE predicate locating one row on the target engine:
/// `rowid = ?` for rowid tables, a PK conjunction for WITHOUT ROWID.
fn row_predicate(
    aux_table: &crate::schema::Table,
    pk_cols: &[String],
    n_leading_params: usize,
) -> (Expr, usize) {
    // The engine's anonymous `?` placeholders are NAMED by their Vec
    // index ("0", "1", ... — the lexer's numbering; see
    // collect_parameters), so synthesized parameters carry explicit
    // numeric names.
    if !aux_table.without_rowid {
        let idx = n_leading_params;
        (
            Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(Expr::Column {
                    table: None,
                    name: "rowid".to_string(),
                }),
                right: Box::new(Expr::Parameter(idx.to_string())),
            },
            idx + 1,
        )
    } else {
        // PK conjunction: pk1 = ? AND pk2 = ? AND ...
        let mut terms: Vec<Expr> = Vec::new();
        for (i, c) in pk_cols.iter().enumerate() {
            terms.push(Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(Expr::Column {
                    table: None,
                    name: c.clone(),
                }),
                right: Box::new(Expr::Parameter((n_leading_params + i).to_string())),
            });
        }
        let mut it = terms.into_iter();
        let first = it.next().unwrap_or(Expr::Literal(Value::Integer(1)));
        let expr = it.fold(first, |acc, t| Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(acc),
            right: Box::new(t),
        });
        (expr, n_leading_params + pk_cols.len())
    }
}

/// The candidate SELECT for UPDATE/DELETE: `SELECT u.rowid AS __rrowid,
/// u.* FROM aux.t AS u [<FROM>] [WHERE ...]` (WITHOUT ROWID tables
/// project the PK columns as the leading key columns instead) — run
/// on the TARGET engine (mixed statements get parent-side tables
/// injected through the reverse channel), so rowids, WHERE semantics
/// and UPDATE-FROM matching all resolve where the table lives.
fn candidate_select(
    aux_table: &crate::schema::Table,
    pk_cols: &[String],
    schema_lc: &str,
    upd_like: &UpdateStatement,
) -> Statement {
    let (table, alias, from, where_clause) = (
        upd_like.table.clone(),
        upd_like.alias.clone(),
        upd_like.from.clone(),
        upd_like.where_clause.clone(),
    );
    let target = TableExpression::Table {
        name: table.clone(),
        schema: Some(schema_lc.to_string()),
        alias: alias.clone(),
        indexed: None,
    };
    let from = match from {
        Some(f) => TableExpression::Join {
            left: Box::new(target),
            right: Box::new(f),
            join_type: JoinType::Cross,
            constraint: JoinConstraint::None,
        },
        None => target,
    };
    let star_target = alias.clone().unwrap_or_else(|| table.clone());
    // Leading key columns: __rrowid for rowid tables, the PK columns
    // (aliased __rpk0..n) for WITHOUT ROWID.
    let mut columns: Vec<ResultColumn> = Vec::new();
    if !aux_table.without_rowid {
        columns.push(ResultColumn::Expr {
            expr: Expr::Column {
                table: Some(star_target.clone()),
                name: "rowid".to_string(),
            },
            alias: Some("__rrowid".to_string()),
        });
    } else {
        for (i, p) in pk_cols.iter().enumerate() {
            columns.push(ResultColumn::Expr {
                expr: Expr::Column {
                    table: Some(star_target.clone()),
                    name: p.clone(),
                },
                alias: Some(format!("__rpk{}", i)),
            });
        }
    }
    columns.push(ResultColumn::TableStar(star_target));
    Statement::Select(SelectStatement {
        with: None,
        body: SelectBody::Simple(SimpleSelect {
            distinct: false,
            columns,
            from: Some(from),
            where_clause,
            group_by: Vec::new(),
            having: None,
            window: Vec::new(),
        }),
        order_by: Vec::new(),
        limit: None,
        offset: None,
    })
}

/// A TEMP trigger on a LOCAL table fires through the executor's normal
/// path; when a body statement ROUTES whole to one attached engine
/// (all-aux references — e.g. `INSERT INTO aux.log VALUES (NEW.x)`),
/// this bridge executes it there through the thread-db pointer (the
/// vtab xConnect bridge pattern). Returns Ok(false) for fully-local
/// bodies AND mixed bodies with a LOCAL target (the caller's
/// in-context, channel-armed path executes those — their local writes
/// stay covered by the firing statement's journal).
///
/// Mixed body statements with a FOREIGN target (write one engine,
/// read another) are rejected with a pointer to the
/// TEMP-trigger-on-the-aux-table form — the driver path supports them
/// natively.
pub(crate) fn try_foreign_body(
    ctx: &crate::executor::ExecContext<'_>,
    stmt: &Statement,
) -> Result<bool> {
    let db_ptr = crate::plugin::abi::current_db_thread();
    if db_ptr.is_null() {
        return Ok(false);
    }
    // SAFETY: ThreadDbGuard installs the pointer for the duration of
    // the statement dispatch that owns `ctx` — the same borrow
    // discipline as the vtab xConnect bridge (the pointer is only
    // dereferenced inside that dispatch).
    let db: &Database = unsafe { &*db_ptr };
    if !db.has_attached() {
        return Ok(false);
    }
    // Only DML / SELECT shapes can route; anything else takes the
    // local path (the executor will produce its own error).
    if !matches!(
        stmt,
        Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) | Statement::Select(_)
    ) {
        return Ok(false);
    }
    match crate::attach::analyze_route(db, stmt) {
        crate::attach::Route::Local => Ok(false),
        crate::attach::Route::Routed { schema_lc } => {
            let stripped = crate::attach::strip_schema_refs(stmt, &schema_lc);
            let params = ctx.params.clone();
            let (cols, rows) =
                db.exec_on_engine_pub(&schema_lc, &stripped, &sql_hint(stmt), &params)?;
            let _ = (cols, rows);
            Ok(true)
        }
        crate::attach::Route::Mixed => {
            // A mixed DML with a FOREIGN target needs the driver's
            // cross-engine machinery (rewrite + channel + per-row
            // writes), which the fire path cannot provide; a mixed
            // statement with a LOCAL target is handled by the caller's
            // channel-armed in-context path.
            let target = crate::attach::target_schema_of(db, stmt);
            if crate::attach::is_foreign_schema(&target) {
                Err(Error::Unsupported(
                    "a TEMP trigger body cannot target an attached table while reading another \
                     database — create the trigger as a TEMP trigger on that attached table \
                     instead",
                ))
            } else {
                Ok(false)
            }
        }
    }
}

/// Validate one cross-database body statement WITHOUT executing it
/// (the first-fire contract: every body statement must resolve before
/// ANY executes): ROUTED statements are prepared on the owning
/// attached engine (`cache_stmt_from_ast` plans them); local and
/// local-target-mixed statements plan through the channel-aware
/// planner.
pub(crate) fn validate_foreign_body(
    ctx: &crate::executor::ExecContext<'_>,
    stmt: &Statement,
) -> Result<()> {
    let db_ptr = crate::plugin::abi::current_db_thread();
    if db_ptr.is_null() {
        // No bridge (no attachments on this engine): the local planner
        // validates.
        let _ =
            crate::api::Database::plan_body_with_channel(ctx.catalog(), stmt, ctx.foreign.clone())?;
        return Ok(());
    }
    // SAFETY: see try_foreign_body.
    let db: &Database = unsafe { &*db_ptr };
    match crate::attach::analyze_route(db, stmt) {
        crate::attach::Route::Routed { schema_lc } => {
            let stripped = crate::attach::strip_schema_refs(stmt, &schema_lc);
            let engine = db
                .attached_engine(&schema_lc)
                .ok_or_else(|| Error::NotFound(format!("unknown database {}", schema_lc)))?;
            let aux = engine.read();
            aux.cache_stmt_from_ast(&stripped)
                .map_err(|e| crate::attach::qualify_routed_error(e, &schema_lc))?;
            Ok(())
        }
        _ => {
            let _ = crate::api::Database::plan_body_with_channel(
                ctx.catalog(),
                stmt,
                ctx.foreign.clone(),
            )?;
            Ok(())
        }
    }
}

/// Arm the executor's foreign channel for a TEMP trigger with foreign
/// references (a LOCAL-table trigger): materializes the WHEN clause's
/// and every body statement's attached tables through the thread-db
/// bridge (attached-engine reads only — safe mid-statement) and
/// installs the merged channel on `ctx` if none is armed. Returns true
/// when the channel was armed here (the caller disarms after the
/// trigger's bodies complete).
pub(crate) fn arm_foreign_channel(
    ctx: &mut crate::executor::ExecContext<'_>,
    trig: &crate::schema::Trigger,
) -> Result<bool> {
    if ctx.foreign.is_some() {
        return Ok(false);
    }
    let db_ptr = crate::plugin::abi::current_db_thread();
    if db_ptr.is_null() {
        return Ok(false);
    }
    // SAFETY: see try_foreign_body.
    let db: &Database = unsafe { &*db_ptr };
    if !db.has_attached() {
        return Ok(false);
    }
    let mut merged = crate::attach::ForeignTables::default();
    for s in &trig.body {
        let f = crate::attach::materialize_foreign(db, s)?;
        merge_foreign(&mut merged, f);
    }
    if let Some(w) = &trig.when_clause {
        let sel = Statement::Select(SelectStatement {
            with: None,
            body: SelectBody::Simple(SimpleSelect::values_row(vec![w.clone()])),
            order_by: Vec::new(),
            limit: None,
            offset: None,
        });
        let f = crate::attach::materialize_foreign(db, &sel)?;
        merge_foreign(&mut merged, f);
    }
    if merged.qualified.is_empty() {
        return Ok(false);
    }
    ctx.foreign = Some(merged);
    Ok(true)
}

fn merge_foreign(into: &mut crate::attach::ForeignTables, from: crate::attach::ForeignTables) {
    for (key, mat) in from.qualified {
        if into.qualified.contains_key(&key) {
            continue;
        }
        into.order.push(key.clone());
        into.qualified.insert(key, mat);
    }
}

/// The routed-DML driver (form a): a DML statement targeting an
/// attached table with live bound TEMP triggers. Returns the
/// statement's RETURNING (cols, rows) — empty when it has none.
pub(crate) fn routed_dml_with_triggers(
    db: &mut Database,
    stmt: &Statement,
    params: &[Value],
    schema_lc: &str,
) -> Result<(Vec<String>, Vec<Row>)> {
    let depth = DRIVER_DEPTH.with(|d| d.get());
    if depth >= MAX_DRIVER_DEPTH {
        return Err(Error::semantic(format!(
            "cross-database trigger recursion exceeded {} levels",
            MAX_DRIVER_DEPTH
        )));
    }
    DRIVER_DEPTH.with(|d| d.set(depth + 1));
    let out = routed_dml_with_triggers_inner(db, stmt, params, schema_lc);
    DRIVER_DEPTH.with(|d| d.set(depth));
    out
}

fn routed_dml_with_triggers_inner(
    db: &mut Database,
    stmt: &Statement,
    params: &[Value],
    schema_lc: &str,
) -> Result<(Vec<String>, Vec<Row>)> {
    // ---- target shape ----
    let (table, event) = match stmt {
        Statement::Insert(i) => (i.table.clone(), TriggerEvent::Insert),
        Statement::Update(u) => (
            u.table.clone(),
            TriggerEvent::Update(u.set.iter().map(|(c, _)| c.clone()).collect()),
        ),
        Statement::Delete(d) => (d.from.clone(), TriggerEvent::Delete),
        _ => {
            return Err(Error::Unsupported(
                "cross-database triggers fire on DML only",
            ))
        }
    };
    let engine = db
        .attached_engine(schema_lc)
        .ok_or_else(|| Error::NotFound(format!("unknown database {}", schema_lc)))?;
    let aux_table = {
        let aux = engine.read();
        aux.catalog_ref()
            .get_table(&table)
            .ok_or_else(|| Error::NotFound(format!("no such table: {}.{}", schema_lc, table)))?
    };
    if aux_table.vtab.is_some() {
        return Err(Error::Unsupported(
            "TEMP triggers on virtual tables in attached databases are not supported",
        ));
    }
    let bound = db.catalog.temp_triggers_bound_to(schema_lc, &table);
    if bound.is_empty() {
        return Err(Error::corruption("driver entered without bound triggers"));
    }
    let col_names: Vec<String> = aux_table.col_names.iter().cloned().collect();
    let has_before = |ev: &TriggerEvent| {
        bound
            .iter()
            .any(|t| t.when == TriggerWhen::Before && event_matches(t, ev))
    };
    let has_after = |ev: &TriggerEvent| {
        bound
            .iter()
            .any(|t| t.when == TriggerWhen::After && event_matches(t, ev))
    };

    // ---- statement scope: parent + every attached engine ----
    db.execute(&format!("SAVEPOINT \"{}\"", SP_NAME), [])?;
    let engines = open_aux_scope(db);
    let mut run = || -> Result<(Vec<String>, Vec<Row>)> {
        driver_body(
            db,
            stmt,
            params,
            schema_lc,
            &engine,
            &aux_table,
            &bound,
            &col_names,
            &event,
            &table,
            has_before(&event),
            has_after(&event),
        )
    };
    match run() {
        Ok(out) => {
            close_aux_scope(&engines, false);
            db.execute(&format!("RELEASE \"{}\"", SP_NAME), [])?;
            Ok(out)
        }
        Err(e) => {
            close_aux_scope(&engines, true);
            let _ = db.execute(&format!("ROLLBACK TO \"{}\"", SP_NAME), []);
            let _ = db.execute(&format!("RELEASE \"{}\"", SP_NAME), []);
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn driver_body(
    db: &mut Database,
    stmt: &Statement,
    params: &[Value],
    schema_lc: &str,
    engine: &Arc<RwLock<Database>>,
    aux_table: &Arc<crate::schema::Table>,
    bound: &[Arc<crate::schema::Trigger>],
    col_names: &[String],
    event: &TriggerEvent,
    table: &str,
    has_before: bool,
    has_after: bool,
) -> Result<(Vec<String>, Vec<Row>)> {
    // Fire triggers in REVERSE creation order (the executor fire path's
    // convention, itself SQLite's trigger-chain order).
    let fire = |db: &mut Database,
                phase: TriggerWhen,
                new_row: Option<&Row>,
                old_row: Option<&Row>|
     -> Result<()> {
        for trig in bound.iter().rev() {
            if trig.when != phase || !event_matches(trig, event) {
                continue;
            }
            if let Some(w) = &trig.when_clause {
                let v = eval_when_guard(db, w, new_row, old_row, col_names, params)?;
                if !v.is_truthy() {
                    continue;
                }
            }
            for s in &trig.body {
                let mut sub = s.clone();
                crate::executor::triggers::substitute_new_old_pub(
                    &mut sub, new_row, old_row, col_names,
                )?;
                db.execute_ast_stmt_pub(&sub, params)?;
            }
        }
        Ok(())
    };

    // Per-row write on the target engine + RETURNING collection.
    let exec_on_target =
        |localized: &Statement, bind: &[Value]| -> Result<(Vec<String>, Vec<Row>)> {
            let aux = engine.write();
            let cached = aux
                .cache_stmt_from_ast(localized)
                .map_err(|e| crate::attach::qualify_routed_error(e, schema_lc))?;
            aux.query_cached_stmt_with_cols_pub(&sql_hint(localized), &cached, bind)
                .map_err(|e| crate::attach::qualify_routed_error(e, schema_lc))
        };

    let pk_cols: Vec<String> = if aux_table.without_rowid {
        crate::schema::without_rowid_pk_columns(aux_table)
            .into_iter()
            .map(|c| c.name)
            .collect()
    } else {
        Vec::new()
    };

    let mut returning_cols: Vec<String> = Vec::new();
    let mut returning_rows: Vec<Row> = Vec::new();
    let mut user_returning = user_returning_of(stmt);

    match stmt {
        // ---------------- INSERT ----------------
        Statement::Insert(ins) => {
            let insert_cols: Vec<String> =
                ins.columns.clone().unwrap_or_else(|| col_names.to_vec());
            // Source rows, evaluated on the parent (SELECT sources may
            // read any database; VALUES arms carry expressions).
            let source_rows: Vec<Vec<Value>> = match &ins.source {
                InsertSource::Values(arms) => {
                    let mut out = Vec::with_capacity(arms.len());
                    for arm in arms {
                        out.push(eval_exprs_at_parent(db, arm, params)?);
                    }
                    out
                }
                InsertSource::Select(sel) => {
                    db.exec_foreign_select(&Statement::Select((**sel).clone()), params)?
                }
                InsertSource::DefaultValues => vec![Vec::new()],
            };
            // A synthetic RETURNING * feeds AFTER triggers the final
            // row (defaults applied, rowid assigned by the target).
            let synthetic_returning = user_returning.is_none() && has_after;
            if synthetic_returning {
                user_returning = Some(vec![ResultColumn::Star]);
            }
            let is_default_values = matches!(ins.source, InsertSource::DefaultValues);
            for provided in source_rows {
                let mapped = map_insert_row(db, aux_table, &insert_cols, &provided, params)?;
                if has_before {
                    fire(db, TriggerWhen::Before, Some(&mapped), None)?;
                }
                let localized = Statement::Insert(InsertStatement {
                    or: ins.or,
                    schema: None,
                    table: table.to_string(),
                    alias: None,
                    columns: if is_default_values {
                        None
                    } else {
                        Some(insert_cols.clone())
                    },
                    source: if is_default_values {
                        InsertSource::DefaultValues
                    } else {
                        InsertSource::Values(vec![(0..provided.len())
                            .map(|i| Expr::Parameter(i.to_string()))
                            .collect()])
                    },
                    upsert: ins.upsert.clone(),
                    returning: user_returning.clone(),
                    with: None,
                });
                let bind: Vec<Value> = if is_default_values {
                    Vec::new()
                } else {
                    provided.clone()
                };
                let (cols, rows) = exec_on_target(&localized, &bind)?;
                if returning_cols.is_empty() {
                    returning_cols = cols;
                }
                let mut it = rows.into_iter();
                let new_row: Row = match it.next() {
                    Some(r) => r,
                    None => mapped.clone(),
                };
                returning_rows.extend(it);
                if has_after {
                    fire(db, TriggerWhen::After, Some(&new_row), None)?;
                }
            }
        }
        // ---------------- UPDATE ----------------
        Statement::Update(upd) => {
            let (cand_cols, cand_rows) =
                candidates(db, params, schema_lc, aux_table, &pk_cols, upd)?;
            let n_key = key_width(aux_table, &pk_cols);
            let _ = &cand_cols;
            for cand in cand_rows {
                if cand.len() < n_key {
                    continue;
                }
                let key: Vec<Value> = cand[..n_key].to_vec();
                let old_row: Row = cand[n_key..].to_vec();
                let new_row = compute_update_row(db, upd, aux_table, &old_row, params)?;
                if has_before {
                    fire(db, TriggerWhen::Before, Some(&new_row), Some(&old_row))?;
                }
                let (pred, _n) = row_predicate(aux_table, &pk_cols, col_names.len());
                let mut bind: Vec<Value> = new_row.clone();
                bind.extend(key.iter().cloned());
                let ret = match (&user_returning, has_after) {
                    (Some(u), _) => Some(u.clone()),
                    (None, true) => Some(vec![ResultColumn::Star]),
                    (None, false) => None,
                };
                let localized = Statement::Update(UpdateStatement {
                    or: upd.or,
                    schema: None,
                    table: table.to_string(),
                    alias: None,
                    set: col_names
                        .iter()
                        .enumerate()
                        .map(|(i, c)| (c.clone(), Expr::Parameter(i.to_string())))
                        .collect(),
                    from: None,
                    where_clause: Some(pred),
                    with: None,
                    returning: ret,
                    order_by: Vec::new(),
                    limit: None,
                });
                let (cols, rows) = exec_on_target(&localized, &bind)?;
                if returning_cols.is_empty() {
                    returning_cols = cols;
                }
                let mut it = rows.into_iter();
                let final_row: Row = match it.next() {
                    Some(r) => r,
                    None => new_row.clone(),
                };
                returning_rows.extend(it);
                if has_after {
                    fire(db, TriggerWhen::After, Some(&final_row), Some(&old_row))?;
                }
            }
        }
        // ---------------- DELETE ----------------
        Statement::Delete(del) => {
            let upd_like = UpdateStatement {
                or: None,
                schema: None,
                table: del.from.clone(),
                alias: del.alias.clone(),
                set: Vec::new(),
                from: None,
                where_clause: del.where_clause.clone(),
                with: None,
                returning: None,
                order_by: Vec::new(),
                limit: None,
            };
            let (_, cand_rows) = candidates(db, params, schema_lc, aux_table, &pk_cols, &upd_like)?;
            let n_key = key_width(aux_table, &pk_cols);
            for cand in cand_rows {
                if cand.len() < n_key {
                    continue;
                }
                let key: Vec<Value> = cand[..n_key].to_vec();
                let old_row: Row = cand[n_key..].to_vec();
                if has_before {
                    fire(db, TriggerWhen::Before, None, Some(&old_row))?;
                }
                let (pred, _n) = row_predicate(aux_table, &pk_cols, 0);
                let localized = Statement::Delete(DeleteStatement {
                    schema: None,
                    from: table.to_string(),
                    alias: None,
                    where_clause: Some(pred),
                    with: None,
                    returning: user_returning.clone(),
                    limit: None,
                    order_by: Vec::new(),
                });
                let (cols, rows) = exec_on_target(&localized, &key)?;
                if returning_cols.is_empty() {
                    returning_cols = cols;
                }
                returning_rows.extend(rows);
                if has_after {
                    fire(db, TriggerWhen::After, None, Some(&old_row))?;
                }
            }
        }
        _ => {}
    }
    Ok((returning_cols, returning_rows))
}

/// The candidate SELECT's leading key-column count: 1 (rowid) or the
/// PK width (WITHOUT ROWID).
fn key_width(aux_table: &crate::schema::Table, pk_cols: &[String]) -> usize {
    if aux_table.without_rowid {
        pk_cols.len()
    } else {
        1
    }
}

/// WHEN guard evaluation through the standard machinery (NEW/OLD row
/// bound like the executor's fire path; the guard expression may carry
/// subqueries that read attached databases — evaluated with the
/// foreign channel armed via the values-select trick).
fn eval_when_guard(
    db: &Database,
    when: &Expr,
    new_row: Option<&Row>,
    old_row: Option<&Row>,
    col_names: &[String],
    params: &[Value],
) -> Result<Value> {
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
    // Substitute NEW.x/OLD.x refs to literals, then evaluate the guard
    // as a one-expression select on the parent.
    let mut e = when.clone();
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
    substitute_new_old_expr(&mut e, &lookup)?;
    let _ = (combined, names);
    // The guard is a CONDITION (sqlite3ExprIfFalse): evaluated as a
    // searched CASE's WHEN, whose jump semantics a bare select-list
    // expression (a value context) would not have.
    let guard = Expr::Case {
        operand: None,
        whens: vec![(e, Expr::Literal(Value::Integer(1)))],
        else_: Some(Box::new(Expr::Literal(Value::Integer(0)))),
    };
    let vs = eval_exprs_at_parent(db, std::slice::from_ref(&guard), params)?;
    Ok(vs.into_iter().next().unwrap_or(Value::Null))
}

/// NEW/OLD substitution for a single expression (the trigger
/// machinery's walker, expression-only).
fn substitute_new_old_expr(
    e: &mut Expr,
    lookup: &dyn Fn(&str, &str) -> Option<Value>,
) -> Result<()> {
    match e {
        Expr::Column {
            table: Some(t),
            name,
        } => {
            if let Some(v) = lookup(t, name) {
                *e = Expr::Literal(v);
            } else if t.eq_ignore_ascii_case("new") || t.eq_ignore_ascii_case("old") {
                return Err(Error::NotFound(format!("no such column: {}.{}", t, name)));
            }
            Ok(())
        }
        Expr::Binary { left, right, .. } => {
            substitute_new_old_expr(left, lookup)?;
            substitute_new_old_expr(right, lookup)
        }
        Expr::Unary { expr, .. } => substitute_new_old_expr(expr, lookup),
        Expr::Between {
            expr, low, high, ..
        } => {
            substitute_new_old_expr(expr, lookup)?;
            substitute_new_old_expr(low, lookup)?;
            substitute_new_old_expr(high, lookup)
        }
        Expr::In { expr, source, .. } => {
            substitute_new_old_expr(expr, lookup)?;
            if let InSource::List(list) = source {
                for item in Arc::make_mut(list).iter_mut() {
                    substitute_new_old_expr(item, lookup)?;
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
            substitute_new_old_expr(expr, lookup)?;
            substitute_new_old_expr(pattern, lookup)?;
            if let Some(es) = escape {
                substitute_new_old_expr(es, lookup)?;
            }
            Ok(())
        }
        Expr::IsNull { expr, .. } => substitute_new_old_expr(expr, lookup),
        Expr::Is { left, right, .. } => {
            substitute_new_old_expr(left, lookup)?;
            substitute_new_old_expr(right, lookup)
        }
        Expr::Function { args, filter, .. } => {
            for a in args {
                substitute_new_old_expr(a, lookup)?;
            }
            if let Some(f) = filter {
                substitute_new_old_expr(f, lookup)?;
            }
            Ok(())
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                substitute_new_old_expr(o, lookup)?;
            }
            for (w, t) in whens {
                substitute_new_old_expr(w, lookup)?;
                substitute_new_old_expr(t, lookup)?;
            }
            if let Some(el) = else_ {
                substitute_new_old_expr(el, lookup)?;
            }
            Ok(())
        }
        Expr::Cast { expr, .. } => substitute_new_old_expr(expr, lookup),
        Expr::Collate { expr, .. } => substitute_new_old_expr(expr, lookup),
        Expr::Row(list) => {
            for item in list {
                substitute_new_old_expr(item, lookup)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Candidate rows for UPDATE/DELETE: the rewritten SELECT runs on the
/// TARGET engine (parent-side tables injected through the reverse
/// channel for mixed statements), returning the leading key column(s)
/// followed by the table's columns in table order.
fn candidates(
    db: &Database,
    params: &[Value],
    schema_lc: &str,
    aux_table: &crate::schema::Table,
    pk_cols: &[String],
    upd_like: &UpdateStatement,
) -> Result<(Vec<String>, Vec<Row>)> {
    let sel = candidate_select(aux_table, pk_cols, schema_lc, upd_like);
    let (rewritten, channel) = crate::attach::prepare_cross_dml(db, &sel, schema_lc)?;
    let engine = db
        .attached_engine(schema_lc)
        .ok_or_else(|| Error::NotFound(format!("unknown database {}", schema_lc)))?;
    let out = {
        let aux = engine.read();
        aux.exec_with_injected_channel(&rewritten, params, channel)
            .map_err(|e| crate::attach::qualify_routed_error(e, schema_lc))?
    };
    Ok(out)
}

fn user_returning_of(stmt: &Statement) -> Option<Vec<ResultColumn>> {
    match stmt {
        Statement::Insert(i) => i.returning.clone(),
        Statement::Update(u) => u.returning.clone(),
        Statement::Delete(d) => d.returning.clone(),
        _ => None,
    }
}

/// Spanning aux-engine savepoints for a LOCAL statement whose target
/// table carries TEMP triggers with foreign references (form b): the
/// dispatcher opens the scope before the statement and finishes it by
/// outcome — aux writes from trigger bodies roll back with the
/// statement.
pub(crate) struct LocalForeignScope {
    engines: Vec<Arc<RwLock<Database>>>,
}

impl LocalForeignScope {
    pub(crate) fn open(db: &Database) -> Self {
        Self {
            engines: open_aux_scope(db),
        }
    }
    pub(crate) fn finish(self) {
        close_aux_scope(&self.engines, false);
    }
    pub(crate) fn abort(self) {
        close_aux_scope(&self.engines, true);
    }
}
