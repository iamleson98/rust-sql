//! ATTACH / DETACH — real attached databases.
//!
//! SQLite's ATTACH gives one connection access to several database files
//! at once, with schema-qualified references (`aux.t`) and cross-database
//! joins. This engine implements the full observable surface through
//! engine federation:
//!
//! - **ATTACH 'file' AS name** opens a REAL second engine (`Database`):
//!   the open path sniffs the magic, so a native `RSQLDB04` container and
//!   a genuine SQLite `.db` (through the sqlite-format bridge) both work.
//!   `':memory:'` attaches an in-memory engine; a missing file is CREATED
//!   (SQLite's own open flags); the same file may be attached twice under
//!   different names.
//! - **Routing**: a statement whose EVERY schema reference lands in one
//!   attached database — `SELECT * FROM aux.t WHERE id = 5`, DML with a
//!   qualified target, DDL (`CREATE TABLE aux.x`, `CREATE INDEX aux.ix`,
//!   `DROP`, `ALTER`, `ANALYZE aux.x`, `REINDEX aux.x`, `VACUUM aux`,
//!   `PRAGMA aux.x`) — is rewritten (schema qualifiers stripped) and run
//!   by that engine's own full machinery: the aux planner picks indexes,
//!   aux fast paths fire, aux transactions journal. Zero federation cost.
//! - **Federation**: statements mixing schemas (`main.t JOIN aux.u`,
//!   `INSERT INTO aux.x SELECT * FROM main.t`) materialize each attached
//!   table through its engine (`SELECT * FROM t`, planned there) and
//!   inject the rows into the parent planner through the same channel
//!   materialized CTEs use (`Plan::CteRows`) — the parent's join /
//!   aggregate / set-op machinery runs unchanged over them.
//! - **Transactions**: BEGIN / COMMIT / ROLLBACK / SAVEPOINT / RELEASE
//!   propagate to every attached engine, so `BEGIN; INSERT INTO aux..;
//!   INSERT INTO main..; ROLLBACK;` undoes both. Commits apply the
//!   attached engines first and main last (sequential, not a SQLite
//!   super-journal — the divergence is documented in the README).
//! - **Name resolution** matches SQLite exactly: unqualified names
//!   search temp → main → attached in ATTACH order; `aux.t.c` three-part
//!   column references resolve; unknown schemas error
//!   `no such table: nosuchdb.t`; unknown PRAGMA schemas error
//!   `unknown database nosuchdb`.
//! - **Error parity** (verified against SQLite 3.53):
//!   `database main is already in use`, `database aux is already in use`,
//!   `too many attached databases - max 10`, `cannot detach database
//!   main`, `no such database: x`, `database aux is locked` (detach of a
//!   db with uncommitted changes inside a transaction), `view v cannot
//!   reference objects in database aux`, `qualified table names are not
//!   allowed on INSERT, UPDATE, and DELETE statements within triggers`,
//!   `no such table: aux.nosuch` (schema-prefixed).
//!
//! Cross-schema DML: an INSERT whose TARGET is attached but whose SOURCE
//! reads local tables is synthesized into a parameterized VALUES insert
//! on the attached engine (rows materialized on main, `?`-bound there),
//! preserving upsert / RETURNING clauses verbatim. UPDATE / DELETE with
//! cross-database `FROM` sources are rejected with a clear message (the
//! SQLite surface there is an anti-pattern federation cannot evaluate
//! incrementally without pushing matches row-by-row).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::api::Database;
use crate::error::{Error, Result};
use crate::sql::ast::*;
use crate::types::{CteMaterialization, Row};

/// SQLite's default `SQLITE_MAX_ATTACHED` (compile-time ceiling, 10).
pub(crate) const MAX_ATTACHED: usize = 10;

/// One attached database: the user-visible name and the real engine
/// behind it. `Arc<RwLock<Database>>` because routed statements need
/// `&mut` (the same outer-write-lock discipline the C ABI and the sqlx
/// pool apply to the primary engine).
pub(crate) struct AttachedDb {
    /// Name as the user typed it (display in `PRAGMA database_list`).
    pub name: String,
    /// Lowercased name (resolution key).
    pub name_lc: String,
    /// The engine: a real `Database` (native or sqlite-format container).
    pub engine: Arc<RwLock<Database>>,
    /// The filename as attached (`':memory:'` for in-memory). Retained
    /// for diagnostics; `PRAGMA database_list` reads the live engine's
    /// path so hot-attaches reflect reality.
    #[allow(dead_code)]
    pub file: String,
}

/// The registry every `Database` carries (`Database::attached`).
pub(crate) type AttachedList = Mutex<Vec<AttachedDb>>;

impl Database {
    /// ATTACH DATABASE expr AS name — the full SQLite semantics.
    pub(crate) fn attach_database(&self, file: &str, name: &str) -> Result<()> {
        let name_lc = name.to_ascii_lowercase();
        if name_lc == "main" {
            return Err(Error::semantic("database main is already in use"));
        }
        {
            let list = self.attached.lock();
            if list.iter().any(|a| a.name_lc == name_lc) {
                return Err(Error::semantic(format!(
                    "database {} is already in use",
                    name
                )));
            }
            if list.len() >= MAX_ATTACHED {
                return Err(Error::semantic(format!(
                    "too many attached databases - max {}",
                    MAX_ATTACHED
                )));
            }
        }
        // Open the engine. ':memory:' (and the anonymous '' form) is an
        // in-memory database; anything else is a file (created when
        // missing — SQLite's ATTACH default is read-write-create).
        let engine: Arc<RwLock<Database>> = if file.is_empty()
            || file.eq_ignore_ascii_case(":memory:")
            || file.eq_ignore_ascii_case("file::memory:")
        {
            Arc::new(RwLock::new(Database::open_in_memory()?))
        } else {
            Arc::new(RwLock::new(Database::open(file)?))
        };
        // ATTACH inside a transaction: the new database joins the
        // transaction (SQLite journals its writes with the rest).
        if self
            .in_transaction
            .load(std::sync::atomic::Ordering::Acquire)
        {
            let mut aux = engine.write();
            aux.execute("BEGIN", [])?;
        }
        self.attached.lock().push(AttachedDb {
            name: name.to_string(),
            name_lc,
            engine,
            file: file.to_string(),
        });
        // Plans may embed attached-table rows; the cache never holds
        // them (attach-aware admission), but a DETACH + re-ATTACH cycle
        // changes what `aux.t` means — clear the last-statement memo so
        // nothing stale leaks.
        self.invalidate_stmt_cache();
        Ok(())
    }

    /// DETACH DATABASE name — SQLite's rules, verified against 3.53:
    /// `main` never detaches; `temp` detaches nothing (it is local);
    /// unknown names error; an attached db with UNCOMMITTED CHANGES
    /// inside an open transaction is `locked`.
    pub(crate) fn detach_database(&self, name: &str) -> Result<()> {
        let name_lc = name.to_ascii_lowercase();
        if name_lc == "main" {
            return Err(Error::semantic("cannot detach database main"));
        }
        if name_lc == "temp" {
            // The temp schema is local (connection-scoped tables). It
            // "exists" when temp objects are present; either way it
            // cannot be detached.
            let has_temp = {
                let cats = self.catalog.all_tables();
                cats.iter().any(|(n, _)| self.catalog.is_temp(n))
            };
            if has_temp {
                return Err(Error::semantic("cannot detach database temp"));
            }
            return Err(Error::NotFound(format!("no such database: {}", name)));
        }
        let entry = {
            let mut list = self.attached.lock();
            let pos = list
                .iter()
                .position(|a| a.name_lc == name_lc)
                .ok_or_else(|| Error::NotFound(format!("no such database: {}", name)))?;
            // Lock rule: inside an open transaction on this connection,
            // a database with uncommitted writes cannot detach.
            if self
                .in_transaction
                .load(std::sync::atomic::Ordering::Acquire)
            {
                // Lock rule: an attached db with UNCOMMITTED WRITES inside
                // the open transaction cannot detach (the __begin__
                // savepoint every transaction carries is machinery, not
                // state — a clean participant detaches freely).
                let aux_dirty = list[pos].engine.read().pager.dirty_page_count() > 0;
                if aux_dirty {
                    return Err(Error::semantic(format!("database {} is locked", name)));
                }
                // Clean participant: roll its (empty) transaction state
                // back so the drop doesn't leave a dangling savepoint.
                let mut aux = list[pos].engine.write();
                let _ = aux.execute("ROLLBACK", []);
            }
            list.remove(pos)
        };
        drop(entry);
        self.invalidate_stmt_cache();
        Ok(())
    }

    /// Snapshot of (display name, engine) in ATTACH order.
    pub(crate) fn attached_snapshot(&self) -> Vec<(String, Arc<RwLock<Database>>)> {
        self.attached
            .lock()
            .iter()
            .map(|a| (a.name.clone(), Arc::clone(&a.engine)))
            .collect()
    }

    /// Resolve a schema name (case-insensitive) to its engine.
    pub(crate) fn attached_engine(&self, schema: &str) -> Option<Arc<RwLock<Database>>> {
        let lc = schema.to_ascii_lowercase();
        self.attached
            .lock()
            .iter()
            .find(|a| a.name_lc == lc)
            .map(|a| Arc::clone(&a.engine))
    }

    /// Number of attached databases.
    #[allow(dead_code)]
    pub(crate) fn attached_count(&self) -> usize {
        self.attached.lock().len()
    }

    /// True when any attached database exists.
    pub(crate) fn has_attached(&self) -> bool {
        !self.attached.lock().is_empty()
    }

    /// BEGIN propagation: every attached engine starts a transaction so
    /// later writes to it journal atomically with main's.
    pub(crate) fn attached_begin(&self) {
        for (_, engine) in self.attached_snapshot() {
            let mut aux = engine.write();
            if !aux
                .in_transaction
                .load(std::sync::atomic::Ordering::Acquire)
            {
                let _ = aux.execute("BEGIN", []);
            }
        }
    }

    /// COMMIT propagation: attached engines commit FIRST, main LAST (the
    /// primary's commit is the anchor a caller observes).
    pub(crate) fn attached_commit(&self) -> Result<()> {
        for (_, engine) in self.attached_snapshot() {
            let mut aux = engine.write();
            if aux
                .in_transaction
                .load(std::sync::atomic::Ordering::Acquire)
            {
                aux.execute("COMMIT", [])?;
            }
        }
        Ok(())
    }

    /// ROLLBACK propagation.
    pub(crate) fn attached_rollback(&self) {
        for (_, engine) in self.attached_snapshot() {
            let mut aux = engine.write();
            if aux
                .in_transaction
                .load(std::sync::atomic::Ordering::Acquire)
            {
                let _ = aux.execute("ROLLBACK", []);
            }
        }
    }

    /// SAVEPOINT / RELEASE / ROLLBACK TO propagation: the same name is
    /// pushed on every attached engine (SQLite's savepoints span all
    /// attached databases).
    pub(crate) fn attached_savepoint(&self, sql: &str) {
        for (_, engine) in self.attached_snapshot() {
            let mut aux = engine.write();
            let _ = aux.execute(sql, []);
        }
    }
}

// ---------------------------------------------------------------------------
// AST walking — schema-reference collection & stripping
// ---------------------------------------------------------------------------

/// One table-reference site, as the resolver sees it.
#[derive(Clone, Debug)]
pub(crate) struct RefSite {
    /// Schema qualifier as written (`Some("aux")` for `aux.t`).
    pub schema: Option<String>,
    /// Table / view / vtab name.
    pub name: String,
    /// The site is a PRAGMA schema qualifier (its unknown-schema error
    /// is `unknown database x`, not the table form).
    pub pragma: bool,
}

/// True for a schema qualifier that must resolve to an ATTACHED db.
pub(crate) fn is_foreign_schema(schema: &Option<String>) -> bool {
    match schema {
        None => false,
        Some(s) => {
            let lc = s.to_ascii_lowercase();
            lc != "main" && lc != "temp"
        }
    }
}

/// Collect every table-reference site in the statement: FROM atoms
/// (including joins, subqueries, CTE bodies, IN-tables), DML targets,
/// DDL targets, PRAGMA / VACUUM / ANALYZE / REINDEX schemas.
pub(crate) fn collect_table_refs(stmt: &Statement, out: &mut Vec<RefSite>) {
    match stmt {
        Statement::Select(s) => collect_select_refs(s, out),
        Statement::Insert(i) => {
            out.push(RefSite {
                schema: i.schema.clone(),
                name: i.table.clone(),
                pragma: false,
            });
            if let Some(w) = &i.with {
                collect_with_refs(w, out);
            }
            match &i.source {
                InsertSource::Select(s) => collect_select_refs(s, out),
                InsertSource::Values(rows) => {
                    for row in rows {
                        for e in row {
                            collect_expr_refs(e, out);
                        }
                    }
                }
                InsertSource::DefaultValues => {}
            }
            if let Some(u) = &i.upsert {
                collect_opt_expr(&u.target_where, out);
                if let UpsertAction::DoUpdate { set, where_clause } = &u.action {
                    for (_, e) in set {
                        collect_expr_refs(e, out);
                    }
                    collect_opt_expr(where_clause, out);
                }
            }
            if let Some(rcs) = &i.returning {
                for rc in rcs {
                    collect_result_column_refs(rc, out);
                }
            }
        }
        Statement::Update(u) => {
            out.push(RefSite {
                schema: u.schema.clone(),
                name: u.table.clone(),
                pragma: false,
            });
            if let Some(w) = &u.with {
                collect_with_refs(w, out);
            }
            if let Some(from) = &u.from {
                collect_table_expr_refs(from, out);
            }
            collect_opt_expr(&u.where_clause, out);
            for (_, e) in &u.set {
                collect_expr_refs(e, out);
            }
            if let Some(rcs) = &u.returning {
                for rc in rcs {
                    collect_result_column_refs(rc, out);
                }
            }
            collect_opt_expr(&u.limit, out);
            for ot in &u.order_by {
                collect_expr_refs(&ot.expr, out);
            }
        }
        Statement::Delete(d) => {
            out.push(RefSite {
                schema: d.schema.clone(),
                name: d.from.clone(),
                pragma: false,
            });
            if let Some(w) = &d.with {
                collect_with_refs(w, out);
            }
            collect_opt_expr(&d.where_clause, out);
            if let Some(rcs) = &d.returning {
                for rc in rcs {
                    collect_result_column_refs(rc, out);
                }
            }
            collect_opt_expr(&d.limit, out);
            for ot in &d.order_by {
                collect_expr_refs(&ot.expr, out);
            }
        }
        Statement::Create(c) => collect_create_refs(c, out),
        Statement::Drop(d) => {
            out.push(RefSite {
                schema: d.schema.clone(),
                name: d.name.clone(),
                pragma: false,
            });
        }
        Statement::Alter(a) => {
            out.push(RefSite {
                schema: a.schema.clone(),
                name: a.table.clone(),
                pragma: false,
            });
        }
        Statement::Explain { inner, .. } => collect_table_refs(inner, out),
        Statement::Pragma(p) => {
            if let Some(s) = &p.schema {
                out.push(RefSite {
                    schema: Some(s.clone()),
                    name: p.name.clone(),
                    pragma: true,
                });
            }
        }
        Statement::Vacuum(v) => {
            if let Some(s) = &v.schema {
                out.push(RefSite {
                    schema: Some(s.clone()),
                    name: String::new(),
                    pragma: false,
                });
            }
        }
        Statement::Reindex {
            schema: Some(s), ..
        }
        | Statement::Analyze {
            schema: Some(s), ..
        } => {
            out.push(RefSite {
                schema: Some(s.clone()),
                name: String::new(),
                pragma: false,
            });
        }
        _ => {}
    }
}

fn collect_create_refs(c: &CreateStatement, out: &mut Vec<RefSite>) {
    match c {
        CreateStatement::Table {
            name, as_select, ..
        } => {
            out.push(RefSite {
                schema: name.schema.clone(),
                name: name.name.clone(),
                pragma: false,
            });
            if let Some(sel) = as_select {
                collect_select_refs(sel, out);
            }
        }
        CreateStatement::Index {
            schema,
            name,
            table,
            columns,
            where_clause,
            ..
        } => {
            // The index's schema scopes BOTH the index name and the
            // table lookup (SQLite: `CREATE INDEX aux.ix ON t(...)` finds
            // t in aux).
            if let Some(s) = schema {
                out.push(RefSite {
                    schema: Some(s.clone()),
                    name: name.clone(),
                    pragma: false,
                });
                out.push(RefSite {
                    schema: Some(s.clone()),
                    name: table.clone(),
                    pragma: false,
                });
            }
            for ic in columns {
                if let Some(e) = &ic.expr {
                    collect_expr_refs(e, out);
                }
            }
            collect_opt_expr(where_clause, out);
        }
        CreateStatement::View { name, select, .. } => {
            out.push(RefSite {
                schema: name.schema.clone(),
                name: name.name.clone(),
                pragma: false,
            });
            collect_select_refs(select, out);
        }
        CreateStatement::Trigger(t) => {
            // The creation target: the trigger's NAME, carrying its
            // schema (unqualified + non-TEMP = MAIN — never the
            // attached fallback; SQLite resolves the trigger's database
            // from the name alone).
            out.push(RefSite {
                schema: t.schema.clone(),
                name: t.name.clone(),
                pragma: false,
            });
            // The ON table resolves WITHIN the trigger's database
            // (bare names scope to it: `CREATE TRIGGER aux.tr ON m`
            // looks up aux.m — SQLite). TEMP triggers may target any
            // database (search order).
            let scope = if t.temp {
                None
            } else {
                Some(t.schema.clone().unwrap_or_else(|| "main".to_string()))
            };
            out.push(RefSite {
                schema: scope,
                name: t.table.clone(),
                pragma: false,
            });
            for s in &t.body {
                collect_table_refs(s, out);
            }
            collect_opt_expr(&t.when_clause, out);
        }
        CreateStatement::VirtualTable { name, .. } => {
            out.push(RefSite {
                schema: name.schema.clone(),
                name: name.name.clone(),
                pragma: false,
            });
        }
    }
}

fn collect_with_refs(w: &WithClause, out: &mut Vec<RefSite>) {
    for cte in &w.ctes {
        collect_select_refs(&cte.select, out);
    }
}

fn collect_select_refs(s: &SelectStatement, out: &mut Vec<RefSite>) {
    if let Some(w) = &s.with {
        collect_with_refs(w, out);
    }
    collect_body_refs(&s.body, out);
    for ot in &s.order_by {
        collect_expr_refs(&ot.expr, out);
    }
    collect_opt_expr(&s.limit, out);
    collect_opt_expr(&s.offset, out);
}

fn collect_body_refs(b: &SelectBody, out: &mut Vec<RefSite>) {
    match b {
        SelectBody::Simple(s) => {
            for rc in &s.columns {
                collect_result_column_refs(rc, out);
            }
            if let Some(from) = &s.from {
                collect_table_expr_refs(from, out);
            }
            collect_opt_expr(&s.where_clause, out);
            for e in &s.group_by {
                collect_expr_refs(e, out);
            }
            collect_opt_expr(&s.having, out);
            for wd in &s.window {
                for e in &wd.partition_by {
                    collect_expr_refs(e, out);
                }
                for ot in &wd.order_by {
                    collect_expr_refs(&ot.expr, out);
                }
            }
        }
        SelectBody::Binary { left, right, .. } => {
            collect_body_refs(left, out);
            collect_body_refs(right, out);
        }
    }
}

fn collect_result_column_refs(rc: &ResultColumn, out: &mut Vec<RefSite>) {
    if let ResultColumn::Expr { expr, .. } = rc {
        collect_expr_refs(expr, out);
    }
}

fn collect_table_expr_refs(te: &TableExpression, out: &mut Vec<RefSite>) {
    match te {
        TableExpression::Table { name, schema, .. } => {
            // ALL FROM sites are collected (unqualified included): the
            // router must see them as LOCAL references (temp → main →
            // attached search order — pre-resolution rewrites the ones
            // only an attached engine answers).
            out.push(RefSite {
                schema: schema.clone(),
                name: name.clone(),
                pragma: false,
            });
        }
        TableExpression::Subquery { select, .. } => collect_select_refs(select, out),
        TableExpression::Join { left, right, .. } => {
            collect_table_expr_refs(left, out);
            collect_table_expr_refs(right, out);
        }
        TableExpression::Function { args, .. } => {
            for a in args {
                collect_expr_refs(a, out);
            }
        }
    }
}

fn collect_opt_expr(o: &Option<Expr>, out: &mut Vec<RefSite>) {
    if let Some(e) = o {
        collect_expr_refs(e, out);
    }
}

fn collect_expr_refs(e: &Expr, out: &mut Vec<RefSite>) {
    match e {
        Expr::Subquery(s) => collect_select_refs(s, out),
        Expr::Exists(s) => collect_select_refs(s, out),
        Expr::Binary { left, right, .. } => {
            collect_expr_refs(left, out);
            collect_expr_refs(right, out);
        }
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => collect_expr_refs(expr, out),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_expr_refs(expr, out);
            collect_expr_refs(low, out);
            collect_expr_refs(high, out);
        }
        Expr::In { expr, source, .. } => {
            collect_expr_refs(expr, out);
            match source {
                InSource::List(list) => {
                    for e in list.iter() {
                        collect_expr_refs(e, out);
                    }
                }
                InSource::Subquery(s) => collect_select_refs(s, out),
                InSource::Table(_) => {}
            }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            collect_expr_refs(expr, out);
            collect_expr_refs(pattern, out);
            if let Some(esc) = escape {
                collect_expr_refs(esc, out);
            }
        }
        Expr::Is { left, right, .. } => {
            collect_expr_refs(left, out);
            collect_expr_refs(right, out);
        }
        Expr::Function {
            args, filter, over, ..
        } => {
            for a in args {
                collect_expr_refs(a, out);
            }
            if let Some(f) = filter {
                collect_expr_refs(f, out);
            }
            if let Some(WindowSpec::Inline(def)) = over.as_deref() {
                for e in &def.partition_by {
                    collect_expr_refs(e, out);
                }
                for ot in &def.order_by {
                    collect_expr_refs(&ot.expr, out);
                }
            }
        }
        Expr::Case {
            operand,
            whens,
            else_,
            ..
        } => {
            if let Some(op) = operand {
                collect_expr_refs(op, out);
            }
            for (w, t) in whens {
                collect_expr_refs(w, out);
                collect_expr_refs(t, out);
            }
            if let Some(el) = else_ {
                collect_expr_refs(el, out);
            }
        }
        Expr::Row(items) => {
            for e in items {
                collect_expr_refs(e, out);
            }
        }
        Expr::Cast { expr, .. } | Expr::Collate { expr, .. } => collect_expr_refs(expr, out),
        Expr::Raise {
            message: Some(m), ..
        } => collect_expr_refs(m, out),
        _ => {}
    }
}

/// Rewrite a statement for ROUTED execution on an attached engine:
/// every reference qualified with `schema` (case-insensitive) becomes
/// local (`None`), so the attached engine's own resolver sees plain
/// names. Returns a deep-cloned, rewritten statement.
pub(crate) fn strip_schema_refs(stmt: &Statement, schema_lc: &str) -> Statement {
    rewrite_refs(stmt, &mut |schema, _name| {
        if ref_matches(schema, schema_lc) {
            *schema = None;
        }
    })
}

/// Deep-clone `stmt` and pass every schema-qualified table site (and
/// every three-part column prefix) through `visit(schema_slot, name)`:
/// `None` out of the slot strips the qualifier (a local name on the
/// executing engine), `Some(s)` sets/renames it. The walk mirrors the
/// reference collector exactly — every position `collect_table_refs`
/// sees is a rewrite position.
pub(crate) fn rewrite_refs<R: FnMut(&mut Option<String>, &str)>(
    stmt: &Statement,
    visit: &mut R,
) -> Statement {
    let mut s = stmt.clone();
    rewrite_statement(&mut s, visit);
    s
}

fn rewrite_statement<R: FnMut(&mut Option<String>, &str)>(stmt: &mut Statement, visit: &mut R) {
    match stmt {
        Statement::Select(s) => rewrite_select(s, visit),
        Statement::Insert(i) => {
            visit(&mut i.schema, &i.table);
            if let Some(w) = &mut i.with {
                rewrite_with(w, visit);
            }
            match &mut i.source {
                InsertSource::Select(s) => rewrite_select(s, visit),
                InsertSource::Values(rows) => {
                    for row in rows {
                        for e in row {
                            rewrite_expr(e, visit);
                        }
                    }
                }
                InsertSource::DefaultValues => {}
            }
            if let Some(u) = &mut i.upsert {
                rewrite_opt_expr(&mut u.target_where, visit);
                if let UpsertAction::DoUpdate { set, where_clause } = &mut u.action {
                    for (_, e) in set {
                        rewrite_expr(e, visit);
                    }
                    rewrite_opt_expr(where_clause, visit);
                }
            }
            if let Some(rcs) = &mut i.returning {
                for rc in rcs {
                    rewrite_result_column(rc, visit);
                }
            }
        }
        Statement::Update(u) => {
            visit(&mut u.schema, &u.table);
            if let Some(w) = &mut u.with {
                rewrite_with(w, visit);
            }
            if let Some(from) = &mut u.from {
                rewrite_table_expr(from, visit);
            }
            rewrite_opt_expr(&mut u.where_clause, visit);
            for (_, e) in &mut u.set {
                rewrite_expr(e, visit);
            }
            if let Some(rcs) = &mut u.returning {
                for rc in rcs {
                    rewrite_result_column(rc, visit);
                }
            }
            rewrite_opt_expr(&mut u.limit, visit);
            for ot in &mut u.order_by {
                rewrite_expr(&mut ot.expr, visit);
            }
        }
        Statement::Delete(d) => {
            visit(&mut d.schema, &d.from);
            if let Some(w) = &mut d.with {
                rewrite_with(w, visit);
            }
            rewrite_opt_expr(&mut d.where_clause, visit);
            if let Some(rcs) = &mut d.returning {
                for rc in rcs {
                    rewrite_result_column(rc, visit);
                }
            }
            rewrite_opt_expr(&mut d.limit, visit);
            for ot in &mut d.order_by {
                rewrite_expr(&mut ot.expr, visit);
            }
        }
        Statement::Create(c) => rewrite_create(c, visit),
        Statement::Drop(d) => {
            visit(&mut d.schema, &d.name);
        }
        Statement::Alter(a) => {
            visit(&mut a.schema, &a.table);
        }
        Statement::Explain { inner, .. } => rewrite_statement(inner, visit),
        Statement::Pragma(p) => {
            visit(&mut p.schema, &p.name);
        }
        Statement::Vacuum(v) => rewrite_opt_schema(&mut v.schema, visit),
        Statement::Reindex { schema, .. } | Statement::Analyze { schema, .. } => {
            rewrite_opt_schema(schema, visit)
        }
        _ => {}
    }
}

fn rewrite_create<R: FnMut(&mut Option<String>, &str)>(c: &mut CreateStatement, visit: &mut R) {
    match c {
        CreateStatement::Table {
            name, as_select, ..
        } => {
            visit(&mut name.schema, &name.name);
            if let Some(sel) = as_select {
                rewrite_select(sel, visit);
            }
        }
        CreateStatement::Index {
            schema,
            columns,
            where_clause,
            ..
        } => {
            visit(schema, "");
            for ic in columns {
                if let Some(e) = &mut ic.expr {
                    rewrite_expr(e, visit);
                }
            }
            rewrite_opt_expr(where_clause, visit);
        }
        CreateStatement::View { name, select, .. } => {
            visit(&mut name.schema, &name.name);
            rewrite_select(select, visit);
        }
        CreateStatement::Trigger(t) => {
            visit(&mut t.schema, &t.name);
            for s in &mut t.body {
                rewrite_statement(s, visit);
            }
            rewrite_opt_expr(&mut t.when_clause, visit);
        }
        CreateStatement::VirtualTable { name, .. } => {
            visit(&mut name.schema, &name.name);
        }
    }
}

fn rewrite_with<R: FnMut(&mut Option<String>, &str)>(w: &mut WithClause, visit: &mut R) {
    for cte in &mut w.ctes {
        rewrite_select(&mut cte.select, visit);
    }
}

fn rewrite_select<R: FnMut(&mut Option<String>, &str)>(s: &mut SelectStatement, visit: &mut R) {
    if let Some(w) = &mut s.with {
        rewrite_with(w, visit);
    }
    rewrite_body(&mut s.body, visit);
    for ot in &mut s.order_by {
        rewrite_expr(&mut ot.expr, visit);
    }
    rewrite_opt_expr(&mut s.limit, visit);
    rewrite_opt_expr(&mut s.offset, visit);
}

fn rewrite_body<R: FnMut(&mut Option<String>, &str)>(b: &mut SelectBody, visit: &mut R) {
    match b {
        SelectBody::Simple(s) => {
            for rc in &mut s.columns {
                rewrite_result_column(rc, visit);
            }
            if let Some(from) = &mut s.from {
                rewrite_table_expr(from, visit);
            }
            rewrite_opt_expr(&mut s.where_clause, visit);
            for e in &mut s.group_by {
                rewrite_expr(e, visit);
            }
            rewrite_opt_expr(&mut s.having, visit);
            for wd in &mut s.window {
                for e in &mut wd.partition_by {
                    rewrite_expr(e, visit);
                }
                for ot in &mut wd.order_by {
                    rewrite_expr(&mut ot.expr, visit);
                }
            }
        }
        SelectBody::Binary { left, right, .. } => {
            rewrite_body(left, visit);
            rewrite_body(right, visit);
        }
    }
}

fn rewrite_result_column<R: FnMut(&mut Option<String>, &str)>(
    rc: &mut ResultColumn,
    visit: &mut R,
) {
    if let ResultColumn::Expr { expr, .. } = rc {
        rewrite_expr(expr, visit);
    }
}

fn rewrite_table_expr<R: FnMut(&mut Option<String>, &str)>(
    te: &mut TableExpression,
    visit: &mut R,
) {
    match te {
        TableExpression::Table { schema, name, .. } => {
            visit(schema, name);
        }
        TableExpression::Subquery { select, .. } => rewrite_select(select, visit),
        TableExpression::Join { left, right, .. } => {
            rewrite_table_expr(left, visit);
            rewrite_table_expr(right, visit);
        }
        TableExpression::Function { args, .. } => {
            for a in args {
                rewrite_expr(a, visit);
            }
        }
    }
}

fn rewrite_opt_expr<R: FnMut(&mut Option<String>, &str)>(o: &mut Option<Expr>, visit: &mut R) {
    if let Some(e) = o {
        rewrite_expr(e, visit);
    }
}

fn rewrite_expr<R: FnMut(&mut Option<String>, &str)>(e: &mut Expr, visit: &mut R) {
    // Three-part column references (`aux.t.c`) pass their schema
    // prefix through the visitor: None drops the prefix (the routed
    // engine sees plain `t.c`), Some(s) swaps it (`main.t.c` ->
    // `<synthetic>.t.c` on the target engine).
    if let Expr::Column { table: Some(q), .. } = e {
        if let Some(dot) = q.find('.') {
            let (sch, rest) = q.split_at(dot);
            let rest = &rest[1..];
            let tname = rest.split('.').next().unwrap_or(rest);
            let mut slot = Some(sch.to_string());
            visit(&mut slot, tname);
            match slot {
                Some(ns) if ns == sch => {} // unchanged
                Some(ns) => *q = format!("{}.{}", ns, rest),
                None => *q = rest.to_string(),
            }
        }
    }
    match e {
        Expr::Subquery(s) => rewrite_select(s, visit),
        Expr::Exists(s) => rewrite_select(s, visit),
        Expr::Binary { left, right, .. } => {
            rewrite_expr(left, visit);
            rewrite_expr(right, visit);
        }
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => rewrite_expr(expr, visit),
        Expr::Between {
            expr, low, high, ..
        } => {
            rewrite_expr(expr, visit);
            rewrite_expr(low, visit);
            rewrite_expr(high, visit);
        }
        Expr::In { expr, source, .. } => {
            rewrite_expr(expr, visit);
            if let InSource::Subquery(s) = source {
                rewrite_select(s, visit);
            }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            rewrite_expr(expr, visit);
            rewrite_expr(pattern, visit);
            if let Some(esc) = escape {
                rewrite_expr(esc, visit);
            }
        }
        Expr::Is { left, right, .. } => {
            rewrite_expr(left, visit);
            rewrite_expr(right, visit);
        }
        Expr::Function {
            args, filter, over, ..
        } => {
            for a in args {
                rewrite_expr(a, visit);
            }
            if let Some(f) = filter {
                rewrite_expr(f, visit);
            }
            if let Some(WindowSpec::Inline(def)) = over.as_deref_mut() {
                for e in &mut def.partition_by {
                    rewrite_expr(e, visit);
                }
                for ot in &mut def.order_by {
                    rewrite_expr(&mut ot.expr, visit);
                }
            }
        }
        Expr::Case {
            operand,
            whens,
            else_,
            ..
        } => {
            if let Some(op) = operand {
                rewrite_expr(op, visit);
            }
            for (w, t) in whens {
                rewrite_expr(w, visit);
                rewrite_expr(t, visit);
            }
            if let Some(el) = else_ {
                rewrite_expr(el, visit);
            }
        }
        Expr::Row(items) => {
            for e in items {
                rewrite_expr(e, visit);
            }
        }
        Expr::Cast { expr, .. } | Expr::Collate { expr, .. } => rewrite_expr(expr, visit),
        Expr::Raise {
            message: Some(m), ..
        } => rewrite_expr(m, visit),
        _ => {}
    }
}

/// Pass a bare schema slot (no table name at this position) through
/// the visitor.
fn rewrite_opt_schema<R: FnMut(&mut Option<String>, &str)>(
    schema: &mut Option<String>,
    visit: &mut R,
) {
    visit(schema, "");
}

fn ref_matches(schema: &Option<String>, schema_lc: &str) -> bool {
    schema
        .as_ref()
        .is_some_and(|s| s.to_ascii_lowercase() == schema_lc)
}

/// Rewrite an attached-engine error for a ROUTED statement: bare
/// `no such table: x` errors carry the schema prefix (SQLite's message
/// shape for schema-qualified references).
pub(crate) fn qualify_routed_error(e: Error, schema: &str) -> Error {
    if let Error::NotFound(m) = &e {
        if let Some(rest) = m.strip_prefix("no such table: ") {
            // The engine's own CREATE INDEX / TRIGGER target errors carry
            // a hardcoded `main.` prefix; strip it before re-qualifying.
            let rest = rest.strip_prefix("main.").unwrap_or(rest);
            return Error::NotFound(format!("no such table: {}.{}", schema, rest));
        }
    }
    e
}

/// Statement-shape rules verified against SQLite 3.53, checked before
/// routing:
/// - a VIEW may not reference objects in another database (both
///   directions; `view v cannot reference objects in database x`);
/// - trigger bodies may not carry schema-qualified DML targets
///   (`qualified table names are not allowed on INSERT, UPDATE, and
///   DELETE statements within triggers`).
pub(crate) fn check_stmt_rules(stmt: &Statement) -> Result<()> {
    if let Statement::Create(CreateStatement::View {
        name, select, temp, ..
    }) = stmt
    {
        if !*temp {
            let own = name
                .schema
                .as_ref()
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_else(|| "main".to_string());
            let mut sites = Vec::new();
            collect_select_refs(select, &mut sites);
            for s in &sites {
                if let Some(sc) = &s.schema {
                    let lc = sc.to_ascii_lowercase();
                    if lc != own && lc != "main" && lc != "temp" {
                        return Err(Error::semantic(format!(
                            "view {} cannot reference objects in database {}",
                            name.name, sc
                        )));
                    }
                    if own != "main" && own != "temp" && (lc == "main" || lc == "temp") {
                        return Err(Error::semantic(format!(
                            "view {} cannot reference objects in database {}",
                            name.name, sc
                        )));
                    }
                }
            }
        }
    }
    if let Statement::Create(CreateStatement::Trigger(t)) = stmt {
        for s in &t.body {
            let qualified_target = match s {
                Statement::Insert(i) => i.schema.is_some(),
                Statement::Update(u) => u.schema.is_some(),
                Statement::Delete(d) => d.schema.is_some(),
                _ => false,
            };
            if qualified_target {
                return Err(Error::semantic(
                    "qualified table names are not allowed on INSERT, UPDATE, and DELETE \
                     statements within triggers",
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Collect the CTE names visible in a statement (for shadowing checks
/// during unqualified pre-resolution).
fn collect_cte_names(stmt: &Statement, out: &mut Vec<String>) {
    fn walk_select(s: &SelectStatement, out: &mut Vec<String>) {
        if let Some(w) = &s.with {
            for cte in &w.ctes {
                out.push(cte.name.clone());
                walk_select(&cte.select, out);
            }
        }
        match &s.body {
            SelectBody::Simple(_) => {}
            SelectBody::Binary { left, right, .. } => {
                walk_body(left, out);
                walk_body(right, out);
            }
        }
    }
    fn walk_body(b: &SelectBody, out: &mut Vec<String>) {
        if let SelectBody::Simple(s) = b {
            if let Some(te) = &s.from {
                walk_expr_te(te, out);
            }
            for c in &s.columns {
                if let ResultColumn::Expr { expr, .. } = c {
                    walk_expr_cte(expr, out);
                }
            }
            collect_opt_cte(&s.where_clause, out);
        }
    }
    fn walk_expr_te(te: &TableExpression, out: &mut Vec<String>) {
        match te {
            TableExpression::Subquery { select, .. } => walk_select(select, out),
            TableExpression::Join { left, right, .. } => {
                walk_expr_te(left, out);
                walk_expr_te(right, out);
            }
            _ => {}
        }
    }
    fn walk_expr_cte(e: &Expr, out: &mut Vec<String>) {
        match e {
            Expr::Subquery(s) | Expr::Exists(s) => walk_select(s, out),
            Expr::Binary { left, right, .. } => {
                walk_expr_cte(left, out);
                walk_expr_cte(right, out);
            }
            _ => {}
        }
    }
    fn collect_opt_cte(o: &Option<Expr>, out: &mut Vec<String>) {
        if let Some(e) = o {
            walk_expr_cte(e, out);
        }
    }
    fn walk_with(w: &WithClause, out: &mut Vec<String>) {
        for cte in &w.ctes {
            out.push(cte.name.clone());
            walk_select(&cte.select, out);
        }
    }
    match stmt {
        Statement::Select(s) => walk_select(s, out),
        Statement::Insert(i) => {
            if let Some(w) = &i.with {
                walk_with(w, out);
            }
            if let InsertSource::Select(sel) = &i.source {
                walk_select(sel, out);
            }
        }
        Statement::Update(u) => {
            if let Some(w) = &u.with {
                walk_with(w, out);
            }
            if let Some(te) = &u.from {
                walk_expr_te(te, out);
            }
        }
        Statement::Delete(d) => {
            if let Some(w) = &d.with {
                walk_with(w, out);
            }
        }
        Statement::Create(c) => {
            if let CreateStatement::Table {
                as_select: Some(sel),
                ..
            } = c
            {
                walk_select(sel, out);
            }
            if let CreateStatement::View { select, .. } = c {
                walk_select(select, out);
            }
        }
        Statement::Explain { inner, .. } => collect_cte_names(inner, out),
        _ => {}
    }
}

/// Does the LOCAL schema resolve an unqualified name (table, view,
/// CTE, eponymous vtab, or a sqlite_schema spelling)?
fn main_resolves(db: &Database, name: &str, ctes: &[String]) -> bool {
    let lc = name.to_ascii_lowercase();
    if lc == "sqlite_master" || lc == "sqlite_schema" || lc == "sqlite_temp_master" {
        return true;
    }
    if lc == "dbstat" {
        return true;
    }
    if ctes.iter().any(|c| c.eq_ignore_ascii_case(name)) {
        return true;
    }
    if db.catalog.get_table(name).is_some() || db.catalog.get_view(name).is_some() {
        return true;
    }
    false
}

/// Collect the statement's table-reference sites with SQLite's
/// resolution rules applied to UNQUALIFIED names when attached
/// databases exist: a name that misses main/temp and hits an attached
/// engine's catalog binds to that schema (temp → main → attached, in
/// ATTACH order). With nothing attached this is the plain site list.
pub(crate) fn resolved_sites(db: &Database, stmt: &Statement) -> Vec<RefSite> {
    let mut sites = Vec::new();
    collect_table_refs(stmt, &mut sites);
    if !db.has_attached() {
        return sites;
    }
    let mut ctes = Vec::new();
    collect_cte_names(stmt, &mut ctes);
    let attached = db.attached_snapshot();
    // CREATE targets never fall back to attached schemas: an unqualified
    // CREATE always creates in MAIN (SQLite); pre-resolution of the
    // target site would send `CREATE TABLE x` into an attached engine
    // that happens to own a same-named table. Creation targets are not
    // lookups.
    let create_target_sites: usize = match stmt {
        Statement::Create(_) => target_site_count(stmt),
        _ => 0,
    };
    for (site_i, site) in sites.iter_mut().enumerate() {
        if site_i < create_target_sites {
            continue;
        }
        if site.schema.is_none() {
            if main_resolves(db, &site.name, &ctes) {
                continue;
            }
            for (an, engine) in &attached {
                let hit = {
                    let aux = engine.read();
                    aux.catalog.get_table(&site.name).is_some()
                        || aux.catalog.get_view(&site.name).is_some()
                };
                if hit {
                    site.schema = Some(an.clone());
                    break;
                }
            }
        }
    }
    sites
}

/// True when the statement participates in ATTACH routing: any schema
/// qualifier names an attached database, or (with databases attached)
/// an unqualified name resolves only on one. Such statements are never
/// admitted to the statement cache — their plan may embed rows
/// materialized from attached engines (per-execution state, like CTE
/// rows) or their routing target can change under DETACH.
pub(crate) fn stmt_uses_attached(db: &Database, stmt: &Statement) -> bool {
    let sites = resolved_sites(db, stmt);
    sites.iter().any(|s| is_foreign_schema(&s.schema))
}

/// Validate that every foreign schema referenced by the statement names
/// an ATTACHED database (SQLite: unknown schemas are
/// `no such table: schema.name`).
pub(crate) fn validate_schemas(db: &Database, stmt: &Statement) -> Result<()> {
    let sites = resolved_sites(db, stmt);
    for s in &sites {
        if let Some(schema) = &s.schema {
            let lc = schema.to_ascii_lowercase();
            if lc != "main" && lc != "temp" && db.attached_engine(schema).is_none() {
                if s.pragma {
                    return Err(Error::semantic(format!("unknown database {}", schema)));
                }
                return Err(Error::NotFound(format!(
                    "no such table: {}.{}",
                    schema, s.name
                )));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Where a statement executes.
pub(crate) enum Route {
    /// Everything (including unqualified names) is local: main / temp.
    Local,
    /// Every foreign reference lands in ONE attached database — route
    /// the whole statement there (schema qualifiers stripped, the
    /// engine's own machinery runs it).
    Routed { schema_lc: String },
    /// References span several schemas (main + attached, or several
    /// attached) — federate: materialize attached tables, plan locally.
    Mixed,
}

/// Classify a statement for routing. The DML/DDL target (first site)
/// decides: a routed target with only same-schema (or unqualified —
/// which then bind to that schema after routing) source refs routes
/// whole; anything else mixes.
pub(crate) fn analyze_route(db: &Database, stmt: &Statement) -> Route {
    let sites = resolved_sites(db, stmt);
    if sites.is_empty() {
        return Route::Local;
    }
    let target_schema: Option<String> = match stmt {
        Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => {
            sites.first().and_then(|s| s.schema.clone())
        }
        Statement::Create(_) | Statement::Drop(_) | Statement::Alter(_) => {
            sites.first().and_then(|s| s.schema.clone())
        }
        Statement::Pragma(p) => p.schema.clone(),
        Statement::Vacuum(v) => v.schema.clone(),
        Statement::Analyze { schema, .. } | Statement::Reindex { schema, .. } => schema.clone(),
        _ => None,
    };
    let mut foreign_schemas: Vec<String> = Vec::new();
    let mut has_local = false;
    for s in &sites {
        match &s.schema {
            Some(sc) => {
                let lc = sc.to_ascii_lowercase();
                if lc == "main" || lc == "temp" {
                    has_local = true;
                } else if !foreign_schemas.contains(&lc) {
                    foreign_schemas.push(lc);
                }
            }
            None => has_local = true,
        }
    }
    if let Some(t) = &target_schema {
        if is_foreign_schema(&Some(t.clone())) {
            let t_lc = t.to_ascii_lowercase();
            // Everything beyond the target's own sites must be either
            // unqualified (binds to the target after routing) or in the
            // target's schema — else the statement mixes.
            let target_site_count = target_site_count(stmt);
            // Unqualified SOURCE refs keep the connection's search order
            // (temp → main → attached): after pre-resolution a `None`
            // schema means MAIN answered it, so a routed DML target with
            // an unqualified source MIXES. Stored bodies (VIEW /
            // TRIGGER) are the exception: their unqualified names belong
            // to the target schema.
            let body_is_own_schema = matches!(
                stmt,
                Statement::Create(CreateStatement::View { .. })
                    | Statement::Create(CreateStatement::Trigger(_))
            );
            let mut mixes = false;
            for s in sites.iter().skip(target_site_count) {
                match &s.schema {
                    None if body_is_own_schema => {}
                    None => {
                        mixes = true;
                        break;
                    }
                    Some(sc) => {
                        let lc = sc.to_ascii_lowercase();
                        if lc != t_lc {
                            mixes = true;
                            break;
                        }
                    }
                }
            }
            if mixes {
                return Route::Mixed;
            }
            return Route::Routed { schema_lc: t_lc };
        }
        // Local target: any foreign source ref -> Mixed.
        if !foreign_schemas.is_empty() {
            return Route::Mixed;
        }
        return Route::Local;
    }
    // SELECT / EXPLAIN / cross-schema reads.
    if foreign_schemas.is_empty() {
        return Route::Local;
    }
    if foreign_schemas.len() == 1 && !has_local {
        return Route::Routed {
            schema_lc: foreign_schemas[0].clone(),
        };
    }
    Route::Mixed
}

/// The schema a DML/DDL/PRAGMA statement's TARGET binds to (resolved
/// through the site list, so unqualified aux-only targets count).
pub(crate) fn target_schema_of(db: &Database, stmt: &Statement) -> Option<String> {
    match stmt {
        Statement::Pragma(p) => p.schema.clone(),
        Statement::Vacuum(v) => v.schema.clone(),
        Statement::Analyze { schema, .. } | Statement::Reindex { schema, .. } => schema.clone(),
        Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => {
            resolved_sites(db, stmt)
                .first()
                .and_then(|s| s.schema.clone())
        }
        Statement::Create(c) => match c {
            CreateStatement::Table { name, .. }
            | CreateStatement::View { name, .. }
            | CreateStatement::VirtualTable { name, .. } => name.schema.clone(),
            CreateStatement::Index { schema, .. } => schema.clone(),
            CreateStatement::Trigger(_) => None,
        },
        Statement::Drop(_) => resolved_sites(db, stmt)
            .first()
            .and_then(|s| s.schema.clone()),
        Statement::Alter(_) => resolved_sites(db, stmt)
            .first()
            .and_then(|s| s.schema.clone()),
        _ => None,
    }
}

/// How many leading sites belong to the DML/DDL TARGET (the collectors
/// push target sites first).
fn target_site_count(stmt: &Statement) -> usize {
    match stmt {
        Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => 1,
        // CREATE INDEX aux.ix ON t: TWO sites (the index name and the
        // table lookup, both scoped by the index's schema).
        Statement::Create(CreateStatement::Index { .. }) => 2,
        Statement::Create(_) | Statement::Drop(_) | Statement::Alter(_) => 1,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Foreign-table materialization (the federation channel)
// ---------------------------------------------------------------------------

/// The per-statement set of materialized attached tables, injected into
/// the planner next to the CTE map.
#[derive(Default, Clone)]
pub(crate) struct ForeignTables {
    /// (schema_lc, name_lc) -> materialization, for qualified refs.
    pub qualified: HashMap<(String, String), CteMaterialization>,
    /// Attach-ordered (schema_lc, name_lc) pairs for the UNQUALIFIED
    /// fallback search (main misses first — SQLite's search order).
    pub order: Vec<(String, String)>,
}

impl ForeignTables {
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.qualified.is_empty()
    }

    /// Resolve `schema.table` (both lowercased by the caller).
    pub fn get_qualified(&self, schema_lc: &str, name_lc: &str) -> Option<&CteMaterialization> {
        self.qualified
            .get(&(schema_lc.to_string(), name_lc.to_string()))
    }

    /// Unqualified fallback: attached databases in ATTACH order.
    pub fn get_unqualified(&self, name_lc: &str) -> Option<&CteMaterialization> {
        for (schema, name) in &self.order {
            if name == name_lc {
                return self.qualified.get(&(schema.clone(), name.clone()));
            }
        }
        None
    }
}

/// Materialize every attached-table reference the statement makes, by
/// running `SELECT * FROM "t"` on the owning engine (full planner: views
/// expand, vtabs connect, sqlite_master resolves — everything that
/// engine can do). Errors carry SQLite's schema-prefixed wording.
pub(crate) fn materialize_foreign(db: &Database, stmt: &Statement) -> Result<ForeignTables> {
    let sites = resolved_sites(db, stmt);
    let mut out = ForeignTables::default();
    if sites.is_empty() {
        return Ok(out);
    }
    let attached = db.attached_snapshot();
    if attached.is_empty() {
        return Ok(out);
    }
    for site in &sites {
        let Some(schema) = &site.schema else { continue };
        let schema_lc = schema.to_ascii_lowercase();
        if schema_lc == "main" || schema_lc == "temp" {
            continue;
        }
        let name_lc = site.name.to_ascii_lowercase();
        let key = (schema_lc.clone(), name_lc.clone());
        if out.qualified.contains_key(&key) {
            continue;
        }
        let Some((_, engine)) = attached
            .iter()
            .find(|(n, _)| n.to_ascii_lowercase() == schema_lc)
        else {
            return Err(Error::NotFound(format!(
                "no such table: {}.{}",
                schema, site.name
            )));
        };
        // sqlite_temp_master is main/temp-only (SQLite: attached schemas
        // have no temp catalog).
        if name_lc == "sqlite_temp_master" || name_lc == "sqlite_temp_schema" {
            return Err(Error::NotFound(format!(
                "no such table: {}.{}",
                schema, site.name
            )));
        }
        let sql = format!("SELECT * FROM \"{}\"", site.name.replace('"', "\"\""));
        let (cols, rows) = {
            let aux = engine.read();
            aux.query_with_columns(&sql, [])
                .map_err(|e| qualify_foreign_error(e, schema, &site.name))?
        };
        // The materialized column set is the engine's own output —
        // bare names, exactly what a subquery exposes.
        let columns: Arc<[String]> = cols.into();
        let rows: Arc<Vec<Row>> = Arc::new(rows);
        out.qualified.insert(key.clone(), (rows, columns));
        out.order.push(key);
    }
    Ok(out)
}

/// Rewrite an attached-engine error to carry the schema prefix when it
/// is a table-not-found error (`no such table: x` ->
/// `no such table: aux.x`) — SQLite's message shape for qualified refs.
fn qualify_foreign_error(e: Error, schema: &str, name: &str) -> Error {
    let bare = format!("no such table: {}", name);
    if let Error::NotFound(m) = &e {
        if m.eq_ignore_ascii_case(&bare) {
            return Error::NotFound(format!("no such table: {}.{}", schema, name));
        }
    }
    e
}

// ---------------------------------------------------------------------------
// Cross-schema DML synthesis (INSERT INTO aux.x SELECT ... FROM main.t)
// ---------------------------------------------------------------------------

/// Synthetic schema names that carry the PARENT's main/temp tables
/// through the TARGET engine's foreign channel: the target's planner
/// routes any schema qualifier that is neither its own main/temp nor a
/// CTE through the channel, so `main.t` must ride under a name that can
/// never be a real attachment (ATTACH schema names starting with
/// `__rsql` are rejected by the parser's identifier rules the same way
/// SQLite rejects impossible names — and no real attachment can produce
/// them because attach names come from user SQL).
pub(crate) const XSRC_MAIN: &str = "__rsqlsrc_main";
pub(crate) const XSRC_TEMP: &str = "__rsqlsrc_temp";

fn synthetic_for(schema_lc: &str) -> &'static str {
    if schema_lc == "temp" {
        XSRC_TEMP
    } else {
        XSRC_MAIN
    }
}

/// Prepare a mixed cross-schema UPDATE/DELETE (target on an attached
/// engine, reads from main / temp / other attached schemas) for
/// execution ON the target engine: rewrite every non-target table
/// reference to a channel-qualified name (main/temp under the synthetic
/// schemas, other attached schemas under their own names — the target
/// planner routes those through the channel), materialize those tables
/// into a [`ForeignTables`], and strip the target's own qualifiers.
///
/// Rewrite order matters: the QUALIFY pass runs while target sites still
/// carry their `aux.` qualifier (so they are never renamed), then the
/// STRIP pass makes them local. The returned statement's unqualified
/// foreign references are gone — every cross-schema read goes through
/// the channel, so the target engine's own tables can never shadow the
/// parent's (SQLite's connection-wide search order, preserved exactly).
pub(crate) fn prepare_cross_dml(
    parent: &Database,
    stmt: &Statement,
    target_lc: &str,
) -> Result<(Statement, ForeignTables)> {
    let sites = resolved_sites(parent, stmt);
    let mut ctes = Vec::new();
    collect_cte_names(stmt, &mut ctes);
    let attached = parent.attached_snapshot();
    // name_lc -> the schema qualifier the ref must carry on the target
    // engine (synthetic for main/temp, the attached name otherwise).
    let mut rename: HashMap<String, String> = HashMap::new();
    // (source schema lc, source name) per entry, for materialization.
    let mut sources: Vec<(String, String)> = Vec::new();
    for site in &sites {
        if site.pragma {
            continue;
        }
        let name_lc = site.name.to_ascii_lowercase();
        if rename.contains_key(&name_lc) {
            continue;
        }
        if ctes.iter().any(|c| c.eq_ignore_ascii_case(&site.name)) {
            // A CTE shadows the name everywhere in this statement.
            continue;
        }
        let src_lc = match &site.schema {
            Some(s) => {
                let lc = s.to_ascii_lowercase();
                if lc == target_lc {
                    continue; // the target side: stays native
                }
                if lc == "main" || lc == "temp" {
                    lc
                } else {
                    // Another attached schema: the target's planner
                    // routes its qualified refs through the channel.
                    lc
                }
            }
            None => {
                // Unqualified and the parent resolved it to MAIN
                // (resolved_sites stamps attached schemas onto their
                // sites; a None schema here means main/temp won —
                // SQLite's search order).
                "main".to_string()
            }
        };
        let channel_schema = if src_lc == "main" || src_lc == "temp" {
            synthetic_for(&src_lc).to_string()
        } else {
            src_lc.clone()
        };
        rename.insert(name_lc, channel_schema);
        sources.push((src_lc, site.name.clone()));
    }

    // Materialize every source table into the channel (rows + columns,
    // evaluated by the OWNING engine so views / vtabs / sqlite_master
    // resolve exactly as the parent sees them).
    let mut channel = ForeignTables::default();
    for (src_lc, name) in &sources {
        let name_lc = name.to_ascii_lowercase();
        let key = (rename[&name_lc].clone(), name_lc.clone());
        if channel.qualified.contains_key(&key) {
            continue;
        }
        let (cols, rows) = if src_lc == "main" || src_lc == "temp" {
            let sql = match name_lc.as_str() {
                "sqlite_master" | "sqlite_schema" => {
                    "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_master".to_string()
                }
                "sqlite_temp_master" | "sqlite_temp_schema" => {
                    "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_temp_master".to_string()
                }
                _ => format!("SELECT * FROM {}.\"{}\"", src_lc, name.replace('"', "\"\"")),
            };
            parent
                .query_with_columns(&sql, [])
                .map_err(|e| qualify_foreign_error(e, src_lc, name))?
        } else {
            let Some((_, engine)) = attached
                .iter()
                .find(|(n, _)| n.to_ascii_lowercase() == *src_lc)
            else {
                return Err(Error::NotFound(format!(
                    "no such table: {}.{}",
                    src_lc, name
                )));
            };
            let sql = format!("SELECT * FROM \"{}\"", name.replace('"', "\"\""));
            {
                let aux = engine.read();
                aux.query_with_columns(&sql, [])
                    .map_err(|e| qualify_foreign_error(e, src_lc, name))?
            }
        };
        let columns: Arc<[String]> = cols.into();
        let rows: Arc<Vec<Row>> = Arc::new(rows);
        channel.qualified.insert(key.clone(), (rows, columns));
        channel.order.push(key);
    }

    // Pass 1 — qualify foreign refs (target sites still aux-qualified,
    // so the visitor never touches them).
    let rewritten = rewrite_refs(stmt, &mut |schema, name| {
        if let Some(dst) = rename.get(&name.to_ascii_lowercase()) {
            let replace = match schema {
                None => true,
                Some(s) => {
                    let lc = s.to_ascii_lowercase();
                    lc == "main" || lc == "temp"
                }
            };
            if replace {
                *schema = Some(dst.clone());
            }
        }
    });
    // Pass 2 — strip the target's own qualifiers (local on the target).
    let rewritten = strip_schema_refs(&rewritten, target_lc);
    Ok((rewritten, channel))
}

/// Synthesize the VALUES-insert for a mixed INSERT: the SELECT source is
/// evaluated on the PARENT engine (foreign channel for any other
/// attached schemas it reads), and the rows become `?`-parameters of a
/// VALUES insert carrying the original upsert / RETURNING clauses.
/// Returns (statement, flattened parameter values).
pub(crate) fn synthesize_cross_insert(
    parent: &Database,
    ins: &InsertStatement,
    params: &[crate::types::Value],
) -> Result<(Statement, Vec<crate::types::Value>)> {
    // Evaluate the source SELECT on the parent (main-first resolution;
    // other attached schemas materialize through the foreign channel).
    let source_rows: Vec<Row> = match &ins.source {
        InsertSource::Select(sel) => {
            parent.exec_foreign_select(&Statement::Select((**sel).clone()), params)?
        }
        // A VALUES source carries no table references, so a mixed
        // classification can only come from the TARGET — routed, not
        // synthesized. Defensive: reject rather than silently drop.
        _ => {
            return Err(Error::Unsupported(
                "cross-database INSERT requires a SELECT source",
            ))
        }
    };
    let mut arms: Vec<Vec<Expr>> = Vec::with_capacity(source_rows.len());
    for row in &source_rows {
        // One `?` per cell; the lexer numbers anonymous placeholders as
        // 0-based Vec indices (see `collect_parameters`).
        let arm: Vec<Expr> = row.iter().map(|_| Expr::Parameter(String::new())).collect();
        arms.push(arm);
    }
    // Number the parameters (0-based indices).
    let mut n = 0usize;
    let mut flat: Vec<crate::types::Value> = Vec::new();
    for (ri, arm) in arms.iter_mut().enumerate() {
        for e in arm.iter_mut() {
            if let Expr::Parameter(p) = e {
                *p = n.to_string();
            }
            n += 1;
        }
        flat.extend(source_rows[ri].iter().cloned());
    }
    let stmt = Statement::Insert(InsertStatement {
        or: ins.or,
        schema: None,
        table: ins.table.clone(),
        alias: None,
        columns: ins.columns.clone(),
        source: InsertSource::Values(arms),
        upsert: ins.upsert.clone(),
        returning: ins.returning.clone(),
        with: None,
    });
    Ok((stmt, flat))
}
