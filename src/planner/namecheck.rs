//! Prepare-time name resolution — SQLite's resolver contract.
//!
//! SQLite resolves every column reference at PREPARE time against the
//! FROM-clause scope chain and errors `no such column: <name-as-written>`
//! before a single row is evaluated. This engine historically deferred
//! resolution to runtime, where an unresolvable name silently evaluated
//! to NULL (a typo'd projection returned a column of NULLs; a typo'd
//! UPDATE SET target bound to slot 0). This module walks the parsed AST
//! once per cache fill and raises the exact SQLite errors:
//!
//! - `no such column: x` / `no such column: t.x` (as written)
//! - `ambiguous column name: x` (two same-level sources expose it)
//! - `no such table: x` (FROM atoms, IN-table sources, `q.*` qualifiers)
//! - `no such index: x` (INDEXED BY, DROP INDEX)
//! - `no such view/trigger: x` (DROP without IF EXISTS)
//! - `no such table: main.x` (CREATE INDEX / CREATE TRIGGER targets)
//! - `no such collation sequence: x` (index + column + ALTER ADD COLUMN)
//! - `unable to identify the object to be reindexed` (REINDEX unknown)
//! - `no such database: x` (DETACH unknown — validated at the exec site)
//! - `cannot join using column x - column not present in both tables`
//!
//! Scope model: a stack of levels, innermost last. Each level carries the
//! FROM-clause sources of one SELECT (or the target table of a DML
//! statement); bare names resolve inner-first, then outward (correlated
//! subqueries). Qualified names bind to a source by alias-or-table-name
//! at the innermost level that has such a source. USING/NATURAL join
//! coalesced columns are exempt from ambiguity (SQLite's merged column).
//! Output aliases are visible to WHERE / GROUP BY / HAVING / ORDER BY
//! (SQLite's documented alias extension). CTE names travel as a separate
//! "visible" list threaded through FROM building (a CTE reference in FROM
//! position resolves through it, including the recursive self-reference).
//!
//! The walk mirrors what the runtime resolver ACCEPTS (suffix matching,
//! case-insensitive compare, rowid spellings on rowid tables, pending
//! virtual tables whose column lists are unknown until xConnect) so a
//! query that would have silently NULL'd errors here, while every query
//! the runtime resolves correctly still passes.

use crate::error::{Error, Result};
use crate::schema::Catalog;
use crate::sql::ast::*;

/// One FROM-clause source as the resolver sees it.
#[derive(Clone)]
struct Source {
    /// The qualifier a `q.col` reference may use (alias, else table name;
    /// `None` for unaliased subqueries — only reachable bare).
    qualifier: Option<String>,
    /// Exposed column names (compared ASCII-insensitively).
    columns: Vec<String>,
    /// rowid / _rowid_ / oid are valid spellings for this source.
    rowid_table: bool,
    /// Qualified-only source: never searched for BARE names (the
    /// upsert `excluded.` pseudo-table — SQLite resolves bare names in
    /// DO UPDATE to the original row only).
    qualified_only: bool,
    /// Pending virtual table (module not yet registered): the column
    /// list is unknown until xConnect — every name is accepted here so
    /// the RUNTIME's `no such module: <name>` fires (SQLite's own
    /// first-use error), not a bogus column error.
    pending_vtab: bool,
}

impl Source {
    fn has_column(&self, name: &str) -> bool {
        self.pending_vtab || self.columns.iter().any(|c| c.eq_ignore_ascii_case(name))
    }
    fn is_rowid_spelling(name: &str) -> bool {
        name.eq_ignore_ascii_case("rowid")
            || name.eq_ignore_ascii_case("_rowid_")
            || name.eq_ignore_ascii_case("oid")
    }
    fn matches_qualifier(&self, q: &str) -> bool {
        self.qualifier
            .as_ref()
            .map(|a| a.eq_ignore_ascii_case(q))
            .unwrap_or(false)
    }
}

/// The resolver scope: SELECT levels, outermost first. Each level also
/// carries the output aliases visible to WHERE/GROUP BY/HAVING/ORDER BY.
#[derive(Clone, Default)]
struct Scope {
    levels: Vec<Level>,
}

#[derive(Clone, Default)]
struct Level {
    sources: Vec<Source>,
    aliases: Vec<String>,
}

impl Scope {
    fn push_level(&mut self) {
        self.levels.push(Level::default());
    }
    fn pop_level(&mut self) {
        self.levels.pop();
    }
    fn cur(&mut self) -> &mut Level {
        self.levels.last_mut().expect("level pushed")
    }

    /// Resolve an unqualified name: innermost level outward.
    fn resolve_bare(&self, name: &str) -> Result<usize> {
        for (depth, level) in self.levels.iter().enumerate().rev() {
            let mut matches = 0usize;
            for src in &level.sources {
                if src.qualified_only {
                    continue;
                }
                if src.has_column(name) {
                    matches += 1;
                }
            }
            if matches == 1 {
                return Ok(depth);
            }
            if matches > 1 {
                return Err(Error::NotFound(format!("ambiguous column name: {}", name)));
            }
            // A real column named "rowid" won above; otherwise the
            // rowid/_rowid_/oid spelling binds to the FIRST rowid-able
            // source at this level (left-to-right, never ambiguous).
            if Source::is_rowid_spelling(name)
                && level
                    .sources
                    .iter()
                    .any(|s| !s.qualified_only && s.rowid_table)
            {
                return Ok(depth);
            }
            // SQLite's alias extension: unqualified names in WHERE /
            // GROUP BY / HAVING / ORDER BY may reference the level's own
            // output aliases (and outer aliases for correlated refs).
            if level.aliases.iter().any(|a| a.eq_ignore_ascii_case(name)) {
                return Ok(depth);
            }
        }
        Err(Error::NotFound(format!("no such column: {}", name)))
    }

    /// Resolve `q.col`: innermost level that HAS a source named q.
    fn resolve_qualified(&self, q: &str, name: &str) -> Result<()> {
        for level in self.levels.iter().rev() {
            for src in &level.sources {
                if !src.matches_qualifier(q) {
                    continue;
                }
                if src.has_column(name) || (src.rowid_table && Source::is_rowid_spelling(name)) {
                    return Ok(());
                }
                return Err(Error::NotFound(format!("no such column: {}.{}", q, name)));
            }
        }
        // Unknown qualifier: SQLite reports the COLUMN-shaped error
        // (`SELECT t2.x FROM t` → "no such column: t2.x").
        Err(Error::NotFound(format!("no such column: {}.{}", q, name)))
    }

    /// Does any level expose a source with this qualifier? (For `q.*`.)
    fn has_qualifier(&self, q: &str) -> bool {
        self.levels
            .iter()
            .rev()
            .any(|l| l.sources.iter().any(|s| s.matches_qualifier(q)))
    }
}

/// Walk context threaded through the validation.
struct Ctx<'a> {
    catalog: &'a Catalog,
    /// Collation names beyond BINARY/NOCASE/RTRIM (connection-registered).
    extra_collations: &'a [String],
    /// Recursion guard for view-in-view validation.
    view_depth: usize,
}

/// Builtin collation set + connection-registered names.
fn collation_exists(ctx: &Ctx<'_>, name: &str) -> bool {
    name.eq_ignore_ascii_case("binary")
        || name.eq_ignore_ascii_case("nocase")
        || name.eq_ignore_ascii_case("rtrim")
        || ctx
            .extra_collations
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name))
}

/// Table-valued function output columns (tableval.rs's fixed schemas).
fn tvf_columns(name: &str) -> Option<Vec<&'static str>> {
    match name.to_ascii_lowercase().as_str() {
        "json_each" | "json_tree" => Some(vec![
            "key", "value", "type", "atom", "id", "parent", "fullkey", "path",
        ]),
        "pragma_table_info" => Some(vec!["cid", "name", "type", "notnull", "dflt_value", "pk"]),
        "pragma_index_list" => Some(vec!["seq", "name", "unique", "origin", "partial"]),
        "pragma_index_info" | "pragma_index_xinfo" => {
            Some(vec!["seqno", "cid", "name", "desc", "coll", "key"])
        }
        "pragma_foreign_key_list" => Some(vec![
            "id",
            "seq",
            "table",
            "from",
            "to",
            "on_update",
            "on_delete",
            "match",
        ]),
        "pragma_collation_list" => Some(vec!["seq", "name"]),
        "pragma_database_list" => Some(vec!["seq", "name", "file"]),
        _ => None,
    }
}

/// A CTE list entry: (name, declared-or-derived output names).
type CteList = Vec<(String, Option<Vec<String>>)>;

/// The empty CTE list (shared by table-scoped expression validation).
static EMPTY_CTES: CteList = Vec::new();

/// A table source's columns (real table, CTE, or view), its
/// rowid-table-ness, and whether the names are UNKNOWABLE (wildcard:
/// complex subquery / uncomputable view outputs — every name is
/// accepted, the engine's historic permissive behavior). `None` when
/// the name is no table/CTE/view at all.
fn table_source(ctx: &Ctx<'_>, name: &str, ctes: &CteList) -> Option<(Vec<String>, bool, bool)> {
    // CTE names shadow catalog tables (WITH's own names first).
    if let Some((_, cols)) = ctes.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Some((cols.clone().unwrap_or_default(), false, false));
    }
    if let Some(t) = ctx.catalog.get_table(name) {
        if t.vtab.as_ref().map(|vt| vt.is_pending()).unwrap_or(false) {
            return Some((Vec::new(), false, true));
        }
        return Some((
            t.columns.iter().map(|c| c.name.clone()).collect(),
            !t.without_rowid,
            false,
        ));
    }
    if let Some(v) = ctx.catalog.get_view(name) {
        match view_output_names(ctx, &v) {
            Some(names) => return Some((names, false, false)),
            None => return Some((Vec::new(), false, true)),
        }
    }
    None
}

/// A view's exposed names: the declared column list, else the body's own
/// output names (leftmost compound arm).
fn view_output_names(ctx: &Ctx<'_>, v: &crate::schema::View) -> Option<Vec<String>> {
    if let Some(list) = &v.columns {
        return Some(list.clone());
    }
    subquery_output_names(ctx, &v.select)
}

/// One SELECT statement's output names (aliases, bare column names,
/// positional columnN for VALUES, star expansion over a plain table).
fn subquery_output_names(ctx: &Ctx<'_>, stmt: &SelectStatement) -> Option<Vec<String>> {
    let ctes = collect_cte_list(ctx, &stmt.with);
    match &stmt.body {
        SelectBody::Simple(s) => simple_names(ctx, s, &ctes),
        SelectBody::Binary { left, .. } => {
            // Compound output names come from the leftmost arm.
            let mut leaf: &SelectBody = left;
            loop {
                match leaf {
                    SelectBody::Simple(s) => return simple_names(ctx, s, &ctes),
                    SelectBody::Binary { left: l, .. } => leaf = l,
                }
            }
        }
    }
}

fn simple_names(ctx: &Ctx<'_>, s: &SimpleSelect, ctes: &CteList) -> Option<Vec<String>> {
    let mut names = Vec::with_capacity(s.columns.len());
    for c in &s.columns {
        match c {
            ResultColumn::Expr { expr, alias } => {
                if let Some(a) = alias {
                    names.push(a.clone());
                } else if let Expr::Column { name, .. } = expr {
                    names.push(name.clone());
                } else {
                    return None;
                }
            }
            ResultColumn::Star | ResultColumn::TableStar(_) => {
                let te = s.from.as_ref()?;
                match te {
                    TableExpression::Table { name, .. } => {
                        let (cols, _, wildcard) = table_source(ctx, name, ctes)?;
                        if wildcard {
                            return None;
                        }
                        names.extend(cols);
                    }
                    TableExpression::Function { name, .. } => {
                        names.extend(tvf_columns(name)?.iter().map(|s| s.to_string()));
                    }
                    // Star over a FROM subquery: the subquery's own output
                    // names (recursively).
                    TableExpression::Subquery { select, .. } => {
                        names.extend(subquery_output_names(ctx, select)?);
                    }
                    // Joins / anything else: uncomputable here.
                    _ => return None,
                }
            }
        }
    }
    Some(names)
}

/// CTE list of a WITH clause: (name, declared-or-derived columns).
fn collect_cte_list(ctx: &Ctx<'_>, with: &Option<WithClause>) -> CteList {
    let mut out = Vec::new();
    let Some(w) = with else { return out };
    for cte in &w.ctes {
        let names = if let Some(list) = &cte.columns {
            Some(list.clone())
        } else {
            subquery_output_names(ctx, &cte.select)
        };
        out.push((cte.name.clone(), names));
    }
    out
}

// ---------------------------------------------------------------------------
// Scoped validation API (trigger first-fire)
// ---------------------------------------------------------------------------

/// A pre-built scope for validating a TRIGGER's WHEN clause and body
/// statements at first fire: the trigger TABLE's columns as a bare
/// source plus the NEW/OLD pseudo-tables (qualified-only, the table's
/// own columns — the substitution step's not-bound-here check catches
/// NEW.x in a DELETE trigger with the same SQLite text).
pub(crate) struct TriggerScope {
    scope: Scope,
}

impl TriggerScope {
    pub(crate) fn new(table: &str, columns: Vec<String>, rowid_table: bool) -> Self {
        let mut scope = Scope::default();
        scope.push_level();
        {
            let level = scope.cur();
            level.sources.push(Source {
                qualifier: Some(table.to_string()),
                columns: columns.clone(),
                rowid_table,
                qualified_only: false,
                pending_vtab: false,
            });
            level.sources.push(Source {
                qualifier: Some("new".to_string()),
                columns: columns.clone(),
                rowid_table,
                qualified_only: true,
                pending_vtab: false,
            });
            level.sources.push(Source {
                qualifier: Some("old".to_string()),
                columns,
                rowid_table,
                qualified_only: true,
                pending_vtab: false,
            });
        }
        TriggerScope { scope }
    }
}

/// Validate one expression against a pre-built scope (trigger WHEN).
pub(crate) fn validate_expr_in_scope(catalog: &Catalog, e: &Expr, ts: &TriggerScope) -> Result<()> {
    let ctx = Ctx {
        catalog,
        extra_collations: &[],
        view_depth: 0,
    };
    let mut scope = ts.scope.clone();
    validate_expr(&ctx, e, &mut scope, &EMPTY_CTES)
}

/// Validate one statement against a pre-built scope (trigger body).
pub(crate) fn validate_stmt_in_scope(
    catalog: &Catalog,
    stmt: &Statement,
    ts: &TriggerScope,
) -> Result<()> {
    let ctx = Ctx {
        catalog,
        extra_collations: &[],
        view_depth: 0,
    };
    let mut scope = ts.scope.clone();
    validate_stmt(&ctx, stmt, &mut scope)
}

// ---------------------------------------------------------------------------
// Statement dispatch
// ---------------------------------------------------------------------------

/// Validate one parsed statement against the catalog. Errors carry
/// SQLite's exact text (rendered verbatim by `Error::NotFound`).
pub fn validate_statement(
    catalog: &Catalog,
    extra_collations: &[String],
    stmt: &Statement,
) -> Result<()> {
    let ctx = Ctx {
        catalog,
        extra_collations,
        view_depth: 0,
    };
    let mut scope = Scope::default();
    validate_stmt(&ctx, stmt, &mut scope)
}

fn validate_stmt(ctx: &Ctx<'_>, stmt: &Statement, scope: &mut Scope) -> Result<()> {
    match stmt {
        Statement::Select(sel) => validate_select(ctx, sel, scope, &EMPTY_CTES),
        Statement::Insert(ins) => validate_insert(ctx, ins, scope),
        Statement::Update(upd) => validate_update(ctx, upd, scope),
        Statement::Delete(del) => validate_delete(ctx, del, scope),
        Statement::Create(c) => validate_create(ctx, c, scope),
        Statement::Drop(d) => validate_drop(ctx, d),
        Statement::Alter(a) => validate_alter(ctx, a),
        Statement::Explain { inner, .. } => validate_stmt(ctx, inner, scope),
        Statement::Analyze { target } => {
            if let Some(name) = target {
                // ANALYZE accepts a TABLE or INDEX name (an index target
                // analyzes its owning table); the unknown-name text is
                // table-shaped.
                if ctx.catalog.get_table(name).is_none() && ctx.catalog.get_index(name).is_none() {
                    return Err(Error::NotFound(format!("no such table: {}", name)));
                }
            }
            Ok(())
        }
        Statement::Reindex { target } => {
            if let Some(name) = target {
                let known =
                    ctx.catalog.get_table(name).is_some() || ctx.catalog.get_index(name).is_some();
                if !known {
                    return Err(Error::NotFound(
                        "unable to identify the object to be reindexed".to_string(),
                    ));
                }
            }
            Ok(())
        }
        // Trigger bodies validate lazily (SQLite's own contract: unknown
        // names inside a body only error when the trigger fires).
        // BEGIN/COMMIT/ROLLBACK/SAVEPOINT/RELEASE/PRAGMA/VACUUM/ATTACH/
        // DETACH: no column references to resolve (DETACH's existence
        // check lives at the exec site, which tracks ATTACH names).
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// SELECT
// ---------------------------------------------------------------------------

/// Validate a WITH clause's CTE bodies: each body sees the earlier CTEs
/// plus itself (recursive), validated against the outer scope.
fn validate_cte_bodies(
    ctx: &Ctx<'_>,
    with: &Option<WithClause>,
    scope: &mut Scope,
) -> Result<CteList> {
    let own = collect_cte_list(ctx, with);
    if let Some(w) = with {
        for (i, cte) in w.ctes.iter().enumerate() {
            let mut body_visible: CteList = Vec::new();
            body_visible.extend(own.iter().take(i + 1).cloned());
            let mut body_scope = scope.clone();
            validate_select(ctx, &cte.select, &mut body_scope, &body_visible)?;
        }
    }
    Ok(own)
}

fn validate_select(
    ctx: &Ctx<'_>,
    stmt: &SelectStatement,
    scope: &mut Scope,
    visible: &CteList,
) -> Result<()> {
    // This SELECT's own WITH names (innermost shadows outer).
    let own = collect_cte_list(ctx, &stmt.with);
    let mut full: CteList = own.clone();
    full.extend(visible.iter().cloned());

    // CTE body validation (each body sees outer + earlier + itself).
    if let Some(w) = &stmt.with {
        for (i, cte) in w.ctes.iter().enumerate() {
            let mut body_visible: CteList = visible.to_vec();
            body_visible.extend(own.iter().take(i + 1).cloned());
            let mut body_scope = scope.clone();
            validate_select(ctx, &cte.select, &mut body_scope, &body_visible)?;
        }
    }

    match &stmt.body {
        SelectBody::Simple(s) => {
            // The FROM level stays pushed through ORDER BY / LIMIT —
            // source columns AND output aliases both resolve (SQLite's
            // alias-first, then-source ORDER BY rule; the WHERE/GROUP/
            // HAVING alias extension was attached inside).
            validate_simple_select(ctx, s, &full, scope)?;
            let r = (|| {
                for t in &stmt.order_by {
                    validate_expr(ctx, &t.expr, scope, &full)?;
                }
                if let Some(l) = &stmt.limit {
                    validate_expr(ctx, l, scope, &full)?;
                }
                if let Some(o) = &stmt.offset {
                    validate_expr(ctx, o, scope, &full)?;
                }
                Ok(())
            })();
            scope.pop_level();
            r
        }
        SelectBody::Binary { .. } => {
            // Compound arms are scope-isolated; ORDER BY at the compound
            // level resolves against OUTPUT NAMES ONLY (SQLite's compound
            // rule — source columns are not visible across arms).
            validate_select_body(ctx, &stmt.body, &full, scope)?;
            scope.push_level();
            scope.cur().aliases = output_aliases_of(&stmt.body);
            let r = (|| {
                for t in &stmt.order_by {
                    validate_expr(ctx, &t.expr, scope, &full)?;
                }
                if let Some(l) = &stmt.limit {
                    validate_expr(ctx, l, scope, &full)?;
                }
                if let Some(o) = &stmt.offset {
                    validate_expr(ctx, o, scope, &full)?;
                }
                Ok(())
            })();
            scope.pop_level();
            r
        }
    }
}

/// The compound's exposed names: the LEFTMOST arm's output names —
/// aliases AND bare column names (`SELECT v FROM a UNION … ORDER BY v`
/// resolves v as an output name, SQLite's compound rule).
fn output_aliases_of(body: &SelectBody) -> Vec<String> {
    let mut leaf: &SelectBody = body;
    loop {
        match leaf {
            SelectBody::Simple(s) => {
                return s
                    .columns
                    .iter()
                    .filter_map(|c| match c {
                        ResultColumn::Expr { alias: Some(a), .. } => Some(a.clone()),
                        ResultColumn::Expr {
                            expr: Expr::Column { name, .. },
                            alias: None,
                        } => Some(name.clone()),
                        _ => None,
                    })
                    .collect()
            }
            SelectBody::Binary { left, .. } => leaf = left.as_ref(),
        }
    }
}

fn validate_select_body(
    ctx: &Ctx<'_>,
    body: &SelectBody,
    ctes: &CteList,
    scope: &mut Scope,
) -> Result<()> {
    // Net-zero level accounting: every Simple leaf's level (pushed and
    // KEPT by validate_simple_select) is popped HERE; Binary arms
    // recurse and balance themselves.
    match body {
        SelectBody::Simple(s) => {
            validate_simple_select(ctx, s, ctes, scope)?;
            scope.pop_level();
            Ok(())
        }
        SelectBody::Binary { left, right, .. } => {
            validate_select_body(ctx, left, ctes, scope)?;
            validate_select_body(ctx, right, ctes, scope)?;
            Ok(())
        }
    }
}

fn validate_simple_select(
    ctx: &Ctx<'_>,
    s: &SimpleSelect,
    ctes: &CteList,
    scope: &mut Scope,
) -> Result<()> {
    // ---- FROM: build this level's sources (table errors first) ----
    // On SUCCESS the level stays pushed (the SELECT-level caller validates
    // ORDER BY / LIMIT in the same scope, then pops); every error path
    // pops before returning.
    scope.push_level();
    if let Err(e) = build_from_sources(ctx, s.from.as_ref(), ctes, scope) {
        scope.pop_level();
        return Err(e);
    }
    // Output aliases are visible to WHERE / GROUP BY / HAVING / ORDER BY
    // (SQLite's documented alias extension — `SELECT x AS y … WHERE y>0`
    // resolves). Attached AFTER sources so a real column always wins.
    scope.cur().aliases = simple_output_aliases(s);
    // ---- WHERE ----
    if let Some(w) = &s.where_clause {
        if let Err(e) = validate_expr(ctx, w, scope, ctes) {
            scope.pop_level();
            return Err(e);
        }
    }
    // ---- GROUP BY ----
    for g in &s.group_by {
        if let Err(e) = validate_expr(ctx, g, scope, ctes) {
            scope.pop_level();
            return Err(e);
        }
    }
    // ---- HAVING ----
    if let Some(h) = &s.having {
        if let Err(e) = validate_expr(ctx, h, scope, ctes) {
            scope.pop_level();
            return Err(e);
        }
    }
    // ---- WINDOW definitions (the WINDOW clause) ----
    for wd in &s.window {
        if let Err(e) = validate_window_def(ctx, wd, scope, ctes) {
            scope.pop_level();
            return Err(e);
        }
    }
    // ---- Projection ----
    for c in &s.columns {
        let res = match c {
            ResultColumn::Star => Ok(()),
            ResultColumn::TableStar(q) => {
                if scope.has_qualifier(q) {
                    Ok(())
                } else {
                    Err(Error::NotFound(format!("no such table: {}", q)))
                }
            }
            ResultColumn::Expr { expr, .. } => validate_expr(ctx, expr, scope, ctes),
        };
        if let Err(e) = res {
            scope.pop_level();
            return Err(e);
        }
    }
    // Level stays pushed — the caller (validate_select) validates ORDER BY
    // / LIMIT in this scope and pops.
    Ok(())
}

/// One SimpleSelect's output aliases (bare names for SQLite's WHERE /
/// GROUP BY / HAVING alias extension).
fn simple_output_aliases(s: &SimpleSelect) -> Vec<String> {
    s.columns
        .iter()
        .filter_map(|c| match c {
            ResultColumn::Expr { alias: Some(a), .. } => Some(a.clone()),
            _ => None,
        })
        .collect()
}

fn validate_window_def(
    ctx: &Ctx<'_>,
    wd: &WindowDef,
    scope: &mut Scope,
    visible: &CteList,
) -> Result<()> {
    for p in &wd.partition_by {
        validate_expr(ctx, p, scope, visible)?;
    }
    for t in &wd.order_by {
        validate_expr(ctx, &t.expr, scope, visible)?;
    }
    if let Some(f) = &wd.frame {
        validate_frame_bound(ctx, &f.start, scope, visible)?;
        if let Some(e) = &f.end {
            validate_frame_bound(ctx, e, scope, visible)?;
        }
    }
    Ok(())
}

fn validate_frame_bound(
    ctx: &Ctx<'_>,
    b: &FrameBound,
    scope: &mut Scope,
    visible: &CteList,
) -> Result<()> {
    match b {
        FrameBound::Preceding(e) | FrameBound::Following(e) => {
            validate_expr(ctx, e, scope, visible)
        }
        _ => Ok(()),
    }
}

/// Build the FROM sources into the CURRENT (already-pushed) scope level.
fn build_from_sources(
    ctx: &Ctx<'_>,
    from: Option<&TableExpression>,
    ctes: &CteList,
    scope: &mut Scope,
) -> Result<()> {
    let Some(te) = from else { return Ok(()) };
    build_from_expr(ctx, te, ctes, scope)?;
    Ok(())
}

/// Recursively append sources; returns the index of the LAST source this
/// sub-expression added (for USING coalescing boundaries).
fn build_from_expr(
    ctx: &Ctx<'_>,
    te: &TableExpression,
    ctes: &CteList,
    scope: &mut Scope,
) -> Result<usize> {
    match te {
        TableExpression::Table {
            name,
            schema,
            alias,
            indexed,
        } => {
            // INDEXED BY: the index must exist AND belong to this table
            // (SQLite reports "no such index" for both mismatches).
            if let Some(IndexedHint::Indexed(idx)) = indexed {
                let owns = ctx
                    .catalog
                    .get_index(idx)
                    .map(|i| i.table.eq_ignore_ascii_case(name))
                    .unwrap_or(false);
                if !owns {
                    return Err(Error::NotFound(format!("no such index: {}", idx)));
                }
            }
            let Some((cols, rowid_table, wildcard)) = table_source(ctx, name, ctes) else {
                let full = match schema {
                    Some(s) => format!("{}.{}", s, name),
                    None => name.clone(),
                };
                return Err(Error::NotFound(format!("no such table: {}", full)));
            };
            // View in FROM: validate the view's body at USE time (SQLite
            // validates lazily at create but resolves at every use).
            if ctx.catalog.get_table(name).is_none()
                && ctes.iter().all(|(n, _)| !n.eq_ignore_ascii_case(name))
                && ctx.catalog.get_view(name).is_some()
            {
                if let Some(v) = ctx.catalog.get_view(name) {
                    validate_view_body(ctx, &v, scope)?;
                }
            }
            let qualifier = alias.clone().or_else(|| Some(name.clone()));
            let n = scope.cur().sources.len();
            scope.cur().sources.push(Source {
                qualifier,
                columns: cols,
                rowid_table,
                qualified_only: false,
                // Wildcard: pending vtab (unknown until xConnect — the
                // planner's `no such module` fires) or uncomputable view
                // outputs — every name accepted.
                pending_vtab: wildcard,
            });
            Ok(n)
        }
        TableExpression::Subquery {
            select,
            alias,
            column_aliases,
        } => {
            // Validate the subquery body with the CURRENT scope as parent
            // (correlated FROM subqueries are legal in SQLite) and the
            // visible CTE list (a subquery atom may name an outer CTE).
            validate_select(ctx, select, scope, ctes)?;
            let mut src = subquery_source(ctx, select, column_aliases);
            src.qualifier = alias.clone();
            let n = scope.cur().sources.len();
            scope.cur().sources.push(src);
            Ok(n)
        }
        TableExpression::Join {
            left,
            right,
            constraint,
            ..
        } => {
            let left_last = build_from_expr(ctx, left, ctes, scope)?;
            let before_right = scope.cur().sources.len();
            let right_first = build_from_expr(ctx, right, ctes, scope)?;
            let _ = right_first;
            match constraint {
                JoinConstraint::Using(names) => {
                    for n in names {
                        let (has_l, has_r) = {
                            let level = scope.cur();
                            let l = level.sources[left_last].has_column(n);
                            let r = level.sources[before_right].has_column(n);
                            (l, r)
                        };
                        if !has_l || !has_r {
                            return Err(Error::NotFound(format!(
                                "cannot join using column {} - column not present in both tables",
                                n
                            )));
                        }
                        // Coalesced: the bare name binds to the LEFT copy;
                        // remove the RIGHT copy from bare search.
                        if let Some(pos) = scope.cur().sources[before_right]
                            .columns
                            .iter()
                            .position(|c| c.eq_ignore_ascii_case(n))
                        {
                            scope.cur().sources[before_right].columns.remove(pos);
                        }
                    }
                }
                JoinConstraint::Natural => {
                    // Common columns coalesce onto the left copy.
                    let common: Vec<String> = {
                        let level = scope.cur();
                        level.sources[before_right]
                            .columns
                            .iter()
                            .filter(|c| level.sources[left_last].has_column(c))
                            .cloned()
                            .collect()
                    };
                    for c in common {
                        if let Some(pos) = scope.cur().sources[before_right]
                            .columns
                            .iter()
                            .position(|x| x.eq_ignore_ascii_case(&c))
                        {
                            scope.cur().sources[before_right].columns.remove(pos);
                        }
                    }
                }
                JoinConstraint::On(e) => validate_expr(ctx, e, scope, ctes)?,
                JoinConstraint::None => {}
            }
            Ok(scope.cur().sources.len().saturating_sub(1))
        }
        TableExpression::Function { name, args, alias } => {
            let fname = name.to_ascii_lowercase();
            let Some(cols) = tvf_columns(&fname) else {
                return Err(Error::semantic(format!(
                    "no such table-valued function: {}",
                    name
                )));
            };
            for a in args {
                validate_expr(ctx, a, scope, ctes)?;
            }
            let n = scope.cur().sources.len();
            scope.cur().sources.push(Source {
                qualifier: alias.clone().or_else(|| Some(name.clone())),
                columns: cols.iter().map(|c| c.to_string()).collect(),
                rowid_table: false,
                qualified_only: false,
                pending_vtab: false,
            });
            Ok(n)
        }
    }
}

/// The FROM-clause subquery atom's exposed names (its declared column
/// aliases override the body's own output names).
fn subquery_source(
    ctx: &Ctx<'_>,
    select: &SelectStatement,
    column_aliases: &Option<Vec<String>>,
) -> Source {
    match column_aliases
        .clone()
        .or_else(|| subquery_output_names(ctx, select))
    {
        Some(columns) => Source {
            qualifier: None, // filled by the caller (alias)
            columns,
            rowid_table: false,
            qualified_only: false,
            pending_vtab: false,
        },
        // Uncomputable output names (complex nested shapes): wildcard —
        // every name accepted (the engine's historic permissiveness for
        // exactly these shapes).
        None => Source {
            qualifier: None,
            columns: Vec::new(),
            rowid_table: false,
            qualified_only: false,
            pending_vtab: true,
        },
    }
}

/// Validate a view's SELECT body (at use time). Depth-guarded: circular
/// views stop validating (names then resolve permissively).
fn validate_view_body(ctx: &Ctx<'_>, v: &crate::schema::View, scope: &mut Scope) -> Result<()> {
    if ctx.view_depth > 16 {
        return Ok(());
    }
    // The view body resolves against an EMPTY outer scope (views are not
    // correlated with their use site) and no CTEs.
    let _ = scope;
    let mut fresh = Scope::default();
    let inner_ctx = Ctx {
        catalog: ctx.catalog,
        extra_collations: ctx.extra_collations,
        view_depth: ctx.view_depth + 1,
    };
    validate_select(&inner_ctx, &v.select, &mut fresh, &EMPTY_CTES)
}

// ---------------------------------------------------------------------------
// Expression walk
// ---------------------------------------------------------------------------

fn validate_expr(ctx: &Ctx<'_>, e: &Expr, scope: &mut Scope, visible: &CteList) -> Result<()> {
    match e {
        Expr::Column { table: None, name } => {
            if name == "*" {
                return Ok(()); // COUNT(*)
            }
            scope.resolve_bare(name)?;
            Ok(())
        }
        Expr::Column {
            table: Some(q),
            name,
        } => {
            if name == "*" {
                if scope.has_qualifier(q) {
                    return Ok(());
                }
                return Err(Error::NotFound(format!("no such table: {}", q)));
            }
            scope.resolve_qualified(q, name)
        }
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Raise { .. } => Ok(()),
        Expr::Binary { left, right, .. } => {
            validate_expr(ctx, left, scope, visible)?;
            validate_expr(ctx, right, scope, visible)
        }
        Expr::Unary { expr, .. } => validate_expr(ctx, expr, scope, visible),
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_expr(ctx, expr, scope, visible)?;
            validate_expr(ctx, low, scope, visible)?;
            validate_expr(ctx, high, scope, visible)
        }
        Expr::In { expr, source, .. } => {
            validate_expr(ctx, expr, scope, visible)?;
            match source {
                InSource::List(items) => {
                    for i in items.iter() {
                        validate_expr(ctx, i, scope, visible)?;
                    }
                    Ok(())
                }
                InSource::Subquery(sel) => {
                    validate_select(ctx, sel, scope, visible)?;
                    Ok(())
                }
                InSource::Table(t) => {
                    if ctx.catalog.get_table(t).is_some() || ctx.catalog.get_view(t).is_some() {
                        Ok(())
                    } else {
                        Err(Error::NotFound(format!("no such table: {}", t)))
                    }
                }
            }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            validate_expr(ctx, expr, scope, visible)?;
            validate_expr(ctx, pattern, scope, visible)?;
            if let Some(es) = escape {
                validate_expr(ctx, es, scope, visible)?;
            }
            Ok(())
        }
        Expr::IsNull { expr, .. } => validate_expr(ctx, expr, scope, visible),
        Expr::Is { left, right, .. } => {
            validate_expr(ctx, left, scope, visible)?;
            validate_expr(ctx, right, scope, visible)
        }
        Expr::Function {
            args, filter, over, ..
        } => {
            for a in args {
                if let Expr::Column { name, .. } = a {
                    if name == "*" {
                        continue;
                    }
                }
                validate_expr(ctx, a, scope, visible)?;
            }
            if let Some(f) = filter {
                validate_expr(ctx, f, scope, visible)?;
            }
            if let Some(o) = over {
                match o.as_ref() {
                    WindowSpec::Named(_) => {} // resolved against the WINDOW clause
                    WindowSpec::Inline(wd) => {
                        validate_window_def(ctx, wd, scope, visible)?;
                    }
                }
            }
            Ok(())
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                validate_expr(ctx, o, scope, visible)?;
            }
            for (w, t) in whens {
                validate_expr(ctx, w, scope, visible)?;
                validate_expr(ctx, t, scope, visible)?;
            }
            if let Some(el) = else_ {
                validate_expr(ctx, el, scope, visible)?;
            }
            Ok(())
        }
        Expr::Row(items) => {
            for i in items {
                validate_expr(ctx, i, scope, visible)?;
            }
            Ok(())
        }
        Expr::Subquery(sel) => validate_select(ctx, sel, scope, visible),
        Expr::Exists(sel) => validate_select(ctx, sel, scope, visible),
        Expr::Cast { expr, .. } => validate_expr(ctx, expr, scope, visible),
        Expr::Collate { expr, .. } => validate_expr(ctx, expr, scope, visible),
    }
}

// ---------------------------------------------------------------------------
// DML: INSERT / UPDATE / DELETE
// ---------------------------------------------------------------------------

/// Target of a DML statement as a resolver source. For a VIEW target,
/// returns the view's columns (view DML through INSTEAD OF triggers).
fn dml_target_source(ctx: &Ctx<'_>, name: &str, alias: Option<&String>) -> Result<Source> {
    if let Some(t) = ctx.catalog.get_table(name) {
        let pending = t.vtab.as_ref().map(|vt| vt.is_pending()).unwrap_or(false);
        return Ok(Source {
            qualifier: alias.cloned().or_else(|| Some(name.to_string())),
            columns: t.columns.iter().map(|c| c.name.clone()).collect(),
            rowid_table: !t.without_rowid,
            qualified_only: false,
            pending_vtab: pending,
        });
    }
    if let Some(v) = ctx.catalog.get_view(name) {
        return Ok(Source {
            qualifier: alias.cloned().or_else(|| Some(name.to_string())),
            columns: view_output_names(ctx, &v).unwrap_or_default(),
            rowid_table: false,
            qualified_only: false,
            // Uncomputable view outputs: every name accepted.
            pending_vtab: view_output_names(ctx, &v).is_none(),
        });
    }
    Err(Error::NotFound(format!("no such table: {}", name)))
}

fn validate_insert(ctx: &Ctx<'_>, ins: &InsertStatement, scope: &mut Scope) -> Result<()> {
    let ctes = validate_cte_bodies(ctx, &ins.with, scope)?;

    let target = dml_target_source(ctx, &ins.table, ins.alias.as_ref())?;

    // Column list (the executor's "table X has no column named Y" stays
    // authoritative for real tables; here we catch VIEW targets, where
    // SQLite checks against the view's exposed columns).
    if ctx.catalog.get_table(&ins.table).is_none() {
        if let Some(cols) = &ins.columns {
            for c in cols {
                if !target.has_column(c) {
                    return Err(Error::NotFound(format!(
                        "table {} has no column named {}",
                        ins.table, c
                    )));
                }
            }
        }
    }

    // Source expression scope: a top-level INSERT's VALUES may contain
    // no column references (the incoming scope is EMPTY there), but a
    // TRIGGER body's INSERT validates against the trigger scope —
    // NEW.x / OLD.x are legal in VALUES and in INSERT ... SELECT (the
    // incoming scope's levels are the parent).
    match &ins.source {
        InsertSource::Values(rows) => {
            for row in rows {
                for v in row {
                    validate_expr(ctx, v, scope, &EMPTY_CTES)?;
                }
            }
        }
        InsertSource::Select(sel) => {
            let mut src_scope = scope.clone();
            validate_select(ctx, sel, &mut src_scope, &ctes)?;
        }
        InsertSource::DefaultValues => {}
    }

    // Upsert.
    if let Some(u) = &ins.upsert {
        // Conflict target: named columns must exist (unknown → column
        // error; existing-but-not-unique is the executor's contract
        // message, checked there).
        for c in &u.target {
            if c.expr.is_some() {
                continue; // expression targets validated by index matching
            }
            if !target.has_column(&c.name) {
                return Err(Error::NotFound(format!("no such column: {}", c.name)));
            }
        }
        if let UpsertAction::DoUpdate { set, where_clause } = &u.action {
            scope.push_level();
            {
                let level = scope.cur();
                level.sources.push(target.clone());
                // `excluded.` pseudo-table: the target's columns,
                // qualified-only.
                level.sources.push(Source {
                    qualifier: Some("excluded".to_string()),
                    columns: target.columns.clone(),
                    rowid_table: target.rowid_table,
                    qualified_only: true,
                    pending_vtab: false,
                });
            }
            for (name, e) in set {
                if !target.has_column(name) {
                    scope.pop_level();
                    return Err(Error::NotFound(format!("no such column: {}", name)));
                }
                validate_expr(ctx, e, scope, &EMPTY_CTES)?;
            }
            if let Some(w) = where_clause {
                validate_expr(ctx, w, scope, &EMPTY_CTES)?;
            }
            scope.pop_level();
        }
    }

    // RETURNING: the target's columns.
    if let Some(rc) = &ins.returning {
        scope.push_level();
        scope.cur().sources.push(target.clone());
        for c in rc {
            let r = match c {
                ResultColumn::Star => Ok(()),
                ResultColumn::TableStar(q) => {
                    if target.matches_qualifier(q) {
                        Ok(())
                    } else {
                        Err(Error::NotFound(format!("no such table: {}", q)))
                    }
                }
                ResultColumn::Expr { expr, .. } => validate_expr(ctx, expr, scope, &EMPTY_CTES),
            };
            if let Err(e) = r {
                scope.pop_level();
                return Err(e);
            }
        }
        scope.pop_level();
    }
    Ok(())
}

fn validate_update(ctx: &Ctx<'_>, upd: &UpdateStatement, scope: &mut Scope) -> Result<()> {
    let ctes = validate_cte_bodies(ctx, &upd.with, scope)?;

    let target = dml_target_source(ctx, &upd.table, upd.alias.as_ref())?;

    // SQLite's UPDATE-FROM resolution order: the TARGET table is a
    // separate, inner scope level — a bare name resolves to the target
    // FIRST (no ambiguity error when both the target and a FROM source
    // expose it; the target wins), then the FROM sources.
    scope.push_level();
    scope.cur().sources.push(target.clone());
    let has_from = upd.from.is_some();
    if has_from {
        scope.push_level();
    }
    // UPDATE ...FROM (SQLite 3.33+): FROM sources join the resolution
    // scope (CTE names shadow catalog tables inside FROM).
    if let Some(from) = &upd.from {
        if let Err(e) = build_from_sources(ctx, Some(from), &ctes, scope) {
            scope.pop_level();
            scope.pop_level();
            return Err(e);
        }
    }
    // SET names (plain columns only — qualified is a parse error the
    // parser already rejects).
    for (name, e) in &upd.set {
        if !target.has_column(name) {
            if has_from {
                scope.pop_level();
            }
            scope.pop_level();
            return Err(Error::NotFound(format!("no such column: {}", name)));
        }
        if let Err(err) = validate_expr(ctx, e, scope, &ctes) {
            if has_from {
                scope.pop_level();
            }
            scope.pop_level();
            return Err(err);
        }
    }
    if let Some(w) = &upd.where_clause {
        if let Err(e) = validate_expr(ctx, w, scope, &ctes) {
            if has_from {
                scope.pop_level();
            }
            scope.pop_level();
            return Err(e);
        }
    }
    for t in &upd.order_by {
        if let Err(e) = validate_expr(ctx, &t.expr, scope, &ctes) {
            if has_from {
                scope.pop_level();
            }
            scope.pop_level();
            return Err(e);
        }
    }
    if let Some(l) = &upd.limit {
        if let Err(e) = validate_expr(ctx, l, scope, &ctes) {
            if has_from {
                scope.pop_level();
            }
            scope.pop_level();
            return Err(e);
        }
    }
    if let Some(rc) = &upd.returning {
        for c in rc {
            let r = match c {
                ResultColumn::Star => Ok(()),
                ResultColumn::TableStar(q) => {
                    if target.matches_qualifier(q) {
                        Ok(())
                    } else {
                        Err(Error::NotFound(format!("no such table: {}", q)))
                    }
                }
                ResultColumn::Expr { expr, .. } => validate_expr(ctx, expr, scope, &ctes),
            };
            if let Err(e) = r {
                if has_from {
                    scope.pop_level();
                }
                scope.pop_level();
                return Err(e);
            }
        }
    }
    if has_from {
        scope.pop_level();
    }
    scope.pop_level();
    Ok(())
}

fn validate_delete(ctx: &Ctx<'_>, del: &DeleteStatement, scope: &mut Scope) -> Result<()> {
    let ctes = validate_cte_bodies(ctx, &del.with, scope)?;

    let target = dml_target_source(ctx, &del.from, del.alias.as_ref())?;
    scope.push_level();
    scope.cur().sources.push(target.clone());
    if let Some(w) = &del.where_clause {
        if let Err(e) = validate_expr(ctx, w, scope, &ctes) {
            scope.pop_level();
            return Err(e);
        }
    }
    for t in &del.order_by {
        if let Err(e) = validate_expr(ctx, &t.expr, scope, &ctes) {
            scope.pop_level();
            return Err(e);
        }
    }
    if let Some(l) = &del.limit {
        if let Err(e) = validate_expr(ctx, l, scope, &ctes) {
            scope.pop_level();
            return Err(e);
        }
    }
    if let Some(rc) = &del.returning {
        for c in rc {
            let r = match c {
                ResultColumn::Star => Ok(()),
                ResultColumn::TableStar(q) => {
                    if target.matches_qualifier(q) {
                        Ok(())
                    } else {
                        Err(Error::NotFound(format!("no such table: {}", q)))
                    }
                }
                ResultColumn::Expr { expr, .. } => validate_expr(ctx, expr, scope, &ctes),
            };
            if let Err(e) = r {
                scope.pop_level();
                return Err(e);
            }
        }
    }
    scope.pop_level();
    Ok(())
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

fn validate_create(ctx: &Ctx<'_>, c: &CreateStatement, scope: &mut Scope) -> Result<()> {
    match c {
        CreateStatement::Table {
            columns,
            constraints,
            as_select,
            ..
        } => {
            // CHECK / generated expressions resolve against THIS table's
            // full column set (SQLite: forward references are legal).
            let mut col_scope = Scope::default();
            col_scope.push_level();
            col_scope.cur().sources.push(Source {
                qualifier: None,
                columns: columns.iter().map(|c| c.name.clone()).collect(),
                rowid_table: true,
                qualified_only: false,
                pending_vtab: false,
            });
            for def in columns {
                for con in &def.constraints {
                    match con {
                        ColumnConstraint::Check(e) => {
                            validate_expr(ctx, e, &mut col_scope, &EMPTY_CTES)?
                        }
                        ColumnConstraint::GeneratedAs { expr, .. } => {
                            validate_expr(ctx, expr, &mut col_scope, &EMPTY_CTES)?
                        }
                        ColumnConstraint::Collate(name) if !collation_exists(ctx, name) => {
                            return Err(Error::NotFound(format!(
                                "no such collation sequence: {}",
                                name
                            )));
                        }
                        ColumnConstraint::Collate(_) => {}
                        _ => {}
                    }
                }
            }
            for con in constraints {
                match con {
                    TableConstraint::Check(e) => {
                        validate_expr(ctx, e, &mut col_scope, &EMPTY_CTES)?
                    }
                    TableConstraint::Unique(cols)
                    | TableConstraint::PrimaryKey { columns: cols } => {
                        for ic in cols {
                            if ic.expr.is_some() {
                                continue; // expression keys validated by build
                            }
                            if !columns
                                .iter()
                                .any(|d| d.name.eq_ignore_ascii_case(&ic.name))
                            {
                                return Err(Error::NotFound(format!(
                                    "no such column: {}",
                                    ic.name
                                )));
                            }
                        }
                    }
                    // REFERENCES clauses are validated lazily (SQLite's
                    // "foreign key mismatch" fires at DML time).
                    TableConstraint::ForeignKey { .. } => {}
                }
            }
            if let Some(sel) = as_select {
                validate_select(ctx, sel, scope, &EMPTY_CTES)?;
            }
            Ok(())
        }
        CreateStatement::Index {
            table,
            columns,
            where_clause,
            ..
        } => {
            let Some(t) = ctx.catalog.get_table(table) else {
                // SQLite's CREATE INDEX target error carries the schema.
                return Err(Error::NotFound(format!("no such table: main.{}", table)));
            };
            let table_cols: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
            for ic in columns {
                if let Some(e) = &ic.expr {
                    // Expression index key: resolve against the table.
                    let mut s = Scope::default();
                    s.push_level();
                    s.cur().sources.push(Source {
                        qualifier: Some(table.clone()),
                        columns: table_cols.clone(),
                        rowid_table: !t.without_rowid,
                        qualified_only: false,
                        pending_vtab: false,
                    });
                    validate_expr(ctx, e, &mut s, &EMPTY_CTES)?;
                } else if !table_cols.iter().any(|c| c.eq_ignore_ascii_case(&ic.name)) {
                    return Err(Error::NotFound(format!("no such column: {}", ic.name)));
                }
                if let Some(coll) = &ic.collation {
                    if !collation_exists(ctx, coll) {
                        return Err(Error::NotFound(format!(
                            "no such collation sequence: {}",
                            coll
                        )));
                    }
                }
            }
            if let Some(w) = where_clause {
                let mut s = Scope::default();
                s.push_level();
                s.cur().sources.push(Source {
                    qualifier: Some(table.clone()),
                    columns: table_cols,
                    rowid_table: !t.without_rowid,
                    qualified_only: false,
                    pending_vtab: false,
                });
                validate_expr(ctx, w, &mut s, &EMPTY_CTES)?;
            }
            Ok(())
        }
        // View bodies validate lazily (SQLite: CREATE VIEW accepts unknown
        // names; the body resolves at first use).
        CreateStatement::View { .. } => Ok(()),
        // Trigger bodies validate lazily, but the TARGET must exist at
        // create time (SQLite: "no such table: main.x") — a TABLE for
        // BEFORE/AFTER triggers, a VIEW for INSTEAD OF triggers.
        CreateStatement::Trigger(tr) => {
            if ctx.catalog.get_table(&tr.table).is_none()
                && ctx.catalog.get_view(&tr.table).is_none()
            {
                return Err(Error::NotFound(format!("no such table: main.{}", tr.table)));
            }
            Ok(())
        }
        CreateStatement::VirtualTable { .. } => Ok(()),
    }
}

fn validate_drop(ctx: &Ctx<'_>, d: &DropStatement) -> Result<()> {
    if d.if_exists {
        return Ok(());
    }
    let present = match d.kind {
        DropKind::Table => ctx.catalog.get_table(&d.name).is_some(),
        DropKind::Index => ctx.catalog.get_index(&d.name).is_some(),
        DropKind::View => ctx.catalog.get_view(&d.name).is_some(),
        DropKind::Trigger => ctx.catalog.get_trigger(&d.name).is_some(),
    };
    if !present {
        let kind = match d.kind {
            DropKind::Table => "table",
            DropKind::Index => "index",
            DropKind::View => "view",
            DropKind::Trigger => "trigger",
        };
        return Err(Error::NotFound(format!("no such {}: {}", kind, d.name)));
    }
    Ok(())
}

fn validate_alter(ctx: &Ctx<'_>, a: &AlterStatement) -> Result<()> {
    let Some(t) = ctx.catalog.get_table(&a.table) else {
        return Err(Error::NotFound(format!("no such table: {}", a.table)));
    };
    match &a.action {
        AlterAction::RenameColumn { old, .. } | AlterAction::DropColumn { name: old } => {
            if !t.columns.iter().any(|c| c.name.eq_ignore_ascii_case(old)) {
                return Err(Error::NotFound(format!("no such column: \"{}\"", old)));
            }
            Ok(())
        }
        AlterAction::RenameTable { .. } => Ok(()),
        AlterAction::AddColumn { column } => {
            // CHECK / generated expressions of the NEW column resolve
            // against the table's columns plus the new one.
            let mut cols: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
            cols.push(column.name.clone());
            let mut s = Scope::default();
            s.push_level();
            s.cur().sources.push(Source {
                qualifier: Some(a.table.clone()),
                columns: cols,
                rowid_table: !t.without_rowid,
                qualified_only: false,
                pending_vtab: false,
            });
            for con in &column.constraints {
                match con {
                    ColumnConstraint::Check(e) => validate_expr(ctx, e, &mut s, &EMPTY_CTES)?,
                    ColumnConstraint::GeneratedAs { expr, .. } => {
                        validate_expr(ctx, expr, &mut s, &EMPTY_CTES)?
                    }
                    ColumnConstraint::Collate(name) if !collation_exists(ctx, name) => {
                        return Err(Error::NotFound(format!(
                            "no such collation sequence: {}",
                            name
                        )));
                    }
                    ColumnConstraint::Collate(_) => {}
                    _ => {}
                }
            }
            Ok(())
        }
    }
}
