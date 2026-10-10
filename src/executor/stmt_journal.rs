//! SQLite's statement-journal decision (`Vdbe::usesStmtJournal`, set by
//! sqlite3VdbeMakeReady from `Parse::isMultiWrite && Parse::mayAbort`).
//!
//! It decides exactly one observable behavior: the SCOPE a "special" error
//! rolls back. sqlite3VdbeHalt rolls back only the failing STATEMENT on
//! SQLITE_FULL / SQLITE_NOMEM when the statement opened a statement
//! journal; otherwise the whole TRANSACTION rolls back and the connection
//! returns to autocommit (a following COMMIT then fails with "cannot
//! commit - no transaction is active"). This engine journals every
//! statement at row granularity, so after such an error the caller asks
//! this model whether SQLite would have widened the rollback.
//!
//! Both flags are set while the statement — and every trigger program it
//! can fire — is CODED, so they are static properties of the statement
//! text and the schema, never of the rows a run touches:
//!
//! * `isMultiWrite` — the statement may write more than one row or table:
//!   INSERT with a SELECT / multi-row VALUES source or INSERT triggers;
//!   UPDATE / DELETE with triggers, foreign-key work, or a WHERE that is
//!   not a single-row (ONEPASS_SINGLE) lookup; REPLACE conflict handling
//!   that deletes index entries or fires delete triggers.
//! * `mayAbort` — some coded check can halt with OE_Abort: a NOT NULL /
//!   CHECK / PRIMARY KEY / UNIQUE constraint whose effective conflict
//!   resolution is ABORT, an immediate foreign key, `RAISE(ABORT)`, or any
//!   non-inline SQL function call (sqlite3VdbeAddFunctionCall marks every
//!   call site, LIKE / GLOB / MATCH / `->` included).

use crate::schema::{Catalog, Table, Trigger};
use crate::sql::ast::{
    BinaryOp, ConflictResolution, DeleteStatement, Expr, ForeignKeyAction, InSource,
    InsertSource, InsertStatement, JoinConstraint, RaiseAction, ResultColumn, SelectBody,
    SelectStatement, Statement, TableExpression, TriggerEvent, UpdateStatement, UpsertAction,
    WithClause,
};
use std::sync::Arc;

/// Would SQLite run `stmt` with a statement journal? `true` also when the
/// model cannot tell (an attached-schema target, an unknown table): the
/// statement-scoped rollback this engine already performs is then kept.
pub(crate) fn uses_stmt_journal(
    catalog: &Catalog,
    stmt: &Statement,
    foreign_keys: bool,
    recursive_triggers: bool,
) -> bool {
    let mut m = Model {
        catalog,
        foreign_keys,
        recursive_triggers,
        multi_write: false,
        may_abort: false,
        unknown: false,
        coded: Vec::new(),
    };
    m.code_stmt(stmt, None);
    m.unknown || (m.multi_write && m.may_abort)
}

struct Model<'a> {
    catalog: &'a Catalog,
    foreign_keys: bool,
    recursive_triggers: bool,
    multi_write: bool,
    may_abort: bool,
    unknown: bool,
    /// Trigger programs already coded (SQLite codes each program once per
    /// statement; also the recursion guard).
    coded: Vec<String>,
}

/// The conflict resolution a constraint check runs under: the statement's
/// (or the firing statement's, for trigger programs) `OR` clause, else the
/// constraint's default — ABORT (column/table-level `ON CONFLICT` clauses
/// are not modeled by the schema).
fn effective(or: Option<ConflictResolution>) -> ConflictResolution {
    or.unwrap_or(ConflictResolution::Abort)
}

impl Model<'_> {
    fn code_stmt(&mut self, stmt: &Statement, outer_or: Option<ConflictResolution>) {
        match stmt {
            Statement::Insert(ins) => self.code_insert(ins, outer_or),
            Statement::Update(upd) => self.code_update(upd, outer_or),
            Statement::Delete(del) => self.code_delete(del, outer_or),
            Statement::Select(sel) => self.scan_select(sel),
            _ => {}
        }
    }

    fn local_table(&mut self, schema: &Option<String>, name: &str) -> Option<Arc<Table>> {
        if let Some(s) = schema {
            if !s.eq_ignore_ascii_case("main") && !s.eq_ignore_ascii_case("temp") {
                self.unknown = true;
                return None;
            }
        }
        self.catalog.get_table(name)
    }

    fn firing_triggers(&self, table: &str, event: &dyn Fn(&TriggerEvent) -> bool) -> Vec<Arc<Trigger>> {
        self.catalog
            .triggers_on_table(table)
            .into_iter()
            .filter(|t| t.events.iter().any(event))
            .collect()
    }

    /// Code the trigger programs (WHEN + body) — their flags land on the
    /// top-level statement (sqlite3ParseToplevel).
    fn code_triggers(&mut self, triggers: &[Arc<Trigger>], or: Option<ConflictResolution>) {
        for t in triggers {
            if self.coded.iter().any(|n| n.eq_ignore_ascii_case(&t.name)) {
                continue;
            }
            self.coded.push(t.name.clone());
            if let Some(w) = &t.when_clause {
                self.scan_expr(w);
            }
            for s in &t.body {
                // codeTriggerProgram: the firing statement's OR clause
                // overrides each step's own unless it is the default.
                let step_or = match s {
                    Statement::Insert(i) => i.or,
                    Statement::Update(u) => u.or,
                    _ => None,
                };
                self.code_stmt(s, or.or(step_or));
            }
        }
    }

    fn has_delete_triggers(&self, table: &str) -> bool {
        !self
            .firing_triggers(table, &|e| matches!(e, TriggerEvent::Delete))
            .is_empty()
    }

    /// Foreign keys whose PARENT is `table` (child table, clause).
    fn fks_referencing(&self, table: &str) -> Vec<(Arc<Table>, crate::schema::ForeignKeyClause)> {
        let mut out = Vec::new();
        if !self.foreign_keys {
            return out;
        }
        for (_, t) in self.catalog.all_tables() {
            for fk in &t.foreign_keys {
                if fk.ref_table.eq_ignore_ascii_case(table) {
                    out.push((t.clone(), fk.clone()));
                }
            }
        }
        out
    }

    /// regTrigCnt in sqlite3GenerateConstraintChecks: REPLACE must run the
    /// full row delete (and so opens a multi-write) when deleting a
    /// conflicting row fires recursive delete triggers or FK actions.
    fn replace_needs_row_delete(&self, table: &Table) -> bool {
        (self.recursive_triggers && self.has_delete_triggers(&table.name))
            || !self.fks_referencing(&table.name).is_empty()
    }

    fn code_insert(&mut self, ins: &InsertStatement, outer_or: Option<ConflictResolution>) {
        let or = outer_or.or(ins.or);
        if let Some(w) = &ins.with {
            self.scan_with(w);
        }
        match &ins.source {
            InsertSource::Values(rows) => {
                if rows.len() > 1 {
                    self.multi_write = true;
                }
                for e in rows.iter().flatten() {
                    self.scan_expr(e);
                }
            }
            InsertSource::Select(sel) => {
                self.multi_write = true;
                self.scan_select(sel);
            }
            InsertSource::DefaultValues => {}
        }
        if let Some(u) = &ins.upsert {
            if let Some(w) = &u.target_where {
                self.scan_expr(w);
            }
            if let UpsertAction::DoUpdate { set, where_clause } = &u.action {
                for (_, e) in set {
                    self.scan_expr(e);
                }
                if let Some(w) = where_clause {
                    self.scan_expr(w);
                }
            }
        }
        if let Some(r) = &ins.returning {
            self.scan_result_columns(r);
        }
        let Some(table) = self.local_table(&ins.schema, &ins.table) else {
            // A view: INSTEAD OF INSERT triggers do all the writing.
            let trg = self.firing_triggers(&ins.table, &|e| matches!(e, TriggerEvent::Insert));
            if trg.is_empty() {
                self.unknown = true;
            } else {
                self.multi_write = true;
                self.code_triggers(&trg, or);
            }
            return;
        };
        let triggers = self.firing_triggers(&table.name, &|e| matches!(e, TriggerEvent::Insert));
        if !triggers.is_empty() {
            self.multi_write = true;
        }
        // Columns the statement does not supply take their DEFAULT, coded
        // inline (a function-valued default is a call site).
        let supplied: Option<Vec<usize>> = ins
            .columns
            .as_ref()
            .map(|cols| cols.iter().filter_map(|c| table.find_column(c)).collect());
        for (i, c) in table.columns.iter().enumerate() {
            let omitted = match (&supplied, &ins.source) {
                (_, InsertSource::DefaultValues) => true,
                (Some(s), _) => !s.contains(&i),
                (None, _) => false,
            };
            if omitted {
                if let Some(d) = &c.default {
                    self.scan_expr(d);
                }
            }
        }
        // Upsert targets: the matching constraint resolves to DO
        // NOTHING / DO UPDATE instead of halting.
        let upsert_cols: Option<Vec<String>> = ins.upsert.as_ref().map(|u| {
            u.target
                .iter()
                .map(|c| c.name.to_ascii_lowercase())
                .collect()
        });
        let upsert_covers = |cols: &[String]| -> bool {
            match &upsert_cols {
                Some(t) if t.is_empty() => true,
                Some(t) => {
                    t.len() == cols.len()
                        && cols.iter().all(|c| t.contains(&c.to_ascii_lowercase()))
                }
                None => false,
            }
        };
        let mode = effective(or);
        // NOT NULL (every column on INSERT; the rowid alias is exempt —
        // a NULL there allocates a rowid).
        for (i, c) in table.columns.iter().enumerate() {
            if c.nullable || table.rowid_alias == Some(i) || c.generated.is_some() {
                continue;
            }
            let on_error = match mode {
                ConflictResolution::Replace if c.default.is_none() => ConflictResolution::Abort,
                m => m,
            };
            if on_error == ConflictResolution::Abort {
                self.may_abort = true;
            }
        }
        self.code_check_constraints(&table, mode, None);
        // Rowid conflict check: coded when the statement supplies the
        // rowid (the alias column — implicitly when there is no column
        // list — or a rowid spelling).
        let pk_chng = !table.without_rowid
            && !matches!(ins.source, InsertSource::DefaultValues)
            && match &ins.columns {
                None => table.rowid_alias.is_some(),
                Some(cols) => cols.iter().any(|c| {
                    match table.find_column(c) {
                        Some(i) => table.rowid_alias == Some(i),
                        None => is_rowid_spelling(c),
                    }
                }),
            };
        if pk_chng {
            let alias_name: Vec<String> = table
                .rowid_alias
                .map(|i| vec![table.columns[i].name.clone()])
                .unwrap_or_default();
            if !(table.rowid_alias.is_some() && upsert_covers(&alias_name)) {
                match mode {
                    ConflictResolution::Abort => self.may_abort = true,
                    ConflictResolution::Replace => {
                        if !self.catalog.indexes_on_table(&table.name).is_empty()
                            || self.replace_needs_row_delete(&table)
                        {
                            self.multi_write = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        // UNIQUE / PRIMARY KEY indexes (a WITHOUT ROWID table's PK is one).
        let mut unique_keys: Vec<Vec<String>> = Vec::new();
        if table.without_rowid {
            unique_keys.push(
                table
                    .columns
                    .iter()
                    .filter(|c| c.primary_key)
                    .map(|c| c.name.clone())
                    .collect(),
            );
        }
        for idx in self.catalog.indexes_on_table(&table.name) {
            for ic in &idx.columns {
                if let Some(e) = &ic.expr {
                    self.scan_expr(e);
                }
            }
            if let Some(p) = &idx.partial_expr {
                self.scan_expr(p);
            }
            if idx.unique {
                unique_keys.push(idx.columns.iter().map(|c| c.name.clone()).collect());
            }
        }
        for key in &unique_keys {
            if upsert_covers(key) {
                continue;
            }
            match mode {
                ConflictResolution::Abort => self.may_abort = true,
                ConflictResolution::Replace if self.replace_needs_row_delete(&table) => {
                    self.multi_write = true
                }
                _ => {}
            }
        }
        // Child-side foreign keys: an immediate constraint can halt.
        if self.foreign_keys && !table.foreign_keys.is_empty() {
            self.may_abort = true;
        }
        self.code_triggers(&triggers, or);
    }

    fn code_check_constraints(
        &mut self,
        table: &Table,
        mode: ConflictResolution,
        changed: Option<&[String]>,
    ) {
        for chk in &table.check_exprs {
            // UPDATE codes only the CHECKs that read a changed column.
            if let Some(ch) = changed {
                let refs = column_refs(chk);
                if !refs.iter().any(|r| ch.iter().any(|c| c.eq_ignore_ascii_case(r))) {
                    continue;
                }
            }
            self.scan_expr(chk);
            // OR IGNORE skips the row; REPLACE has no meaning for a CHECK
            // and halts like ABORT.
            if matches!(mode, ConflictResolution::Abort | ConflictResolution::Replace) {
                self.may_abort = true;
            }
        }
    }

    fn code_update(&mut self, upd: &UpdateStatement, outer_or: Option<ConflictResolution>) {
        let or = outer_or.or(upd.or);
        if let Some(w) = &upd.with {
            self.scan_with(w);
        }
        for (_, e) in &upd.set {
            self.scan_expr(e);
        }
        if let Some(w) = &upd.where_clause {
            self.scan_expr(w);
        }
        if let Some(f) = &upd.from {
            self.scan_from(f);
        }
        if let Some(r) = &upd.returning {
            self.scan_result_columns(r);
        }
        for t in &upd.order_by {
            self.scan_expr(&t.expr);
        }
        if let Some(l) = &upd.limit {
            self.scan_expr(l);
        }
        let changed: Vec<String> = upd.set.iter().map(|(c, _)| c.clone()).collect();
        let fires = |e: &TriggerEvent| match e {
            TriggerEvent::Update(cols) => {
                cols.is_empty()
                    || cols
                        .iter()
                        .any(|c| changed.iter().any(|x| x.eq_ignore_ascii_case(c)))
            }
            _ => false,
        };
        let Some(table) = self.local_table(&upd.schema, &upd.table) else {
            let trg = self.firing_triggers(&upd.table, &fires);
            if trg.is_empty() {
                self.unknown = true;
            } else {
                self.multi_write = true;
                self.code_triggers(&trg, or);
            }
            return;
        };
        let triggers = self.firing_triggers(&table.name, &fires);
        let is_changed = |name: &str| changed.iter().any(|c| c.eq_ignore_ascii_case(name));
        // hasFK (sqlite3FkRequired): a child FK whose columns change, or
        // a parent key other tables reference that changes.
        let child_fk = self.foreign_keys
            && table
                .foreign_keys
                .iter()
                .any(|fk| fk.columns.iter().any(|&i| is_changed(&table.columns[i].name)));
        let parent_fks: Vec<_> = self
            .fks_referencing(&table.name)
            .into_iter()
            .filter(|(_, fk)| {
                if fk.ref_columns.is_empty() {
                    table.columns.iter().any(|c| c.primary_key && is_changed(&c.name))
                        || table.rowid_alias.is_some_and(|i| is_changed(&table.columns[i].name))
                } else {
                    fk.ref_columns.iter().any(|c| is_changed(c))
                }
            })
            .collect();
        if !triggers.is_empty() || child_fk || !parent_fks.is_empty() {
            self.multi_write = true;
        }
        if upd.from.is_some()
            || upd.limit.is_some()
            || !self.where_is_single_row(&table, upd.where_clause.as_ref())
        {
            self.multi_write = true;
        }
        let mode = effective(or);
        for (i, c) in table.columns.iter().enumerate() {
            if c.nullable || table.rowid_alias == Some(i) || !is_changed(&c.name) {
                continue;
            }
            let on_error = match mode {
                ConflictResolution::Replace if c.default.is_none() => ConflictResolution::Abort,
                m => m,
            };
            if on_error == ConflictResolution::Abort {
                self.may_abort = true;
            }
        }
        self.code_check_constraints(&table, mode, Some(&changed));
        let rowid_changed = !table.without_rowid
            && changed.iter().any(|c| match table.find_column(c) {
                Some(i) => table.rowid_alias == Some(i),
                None => is_rowid_spelling(c),
            });
        if rowid_changed {
            match mode {
                ConflictResolution::Abort => self.may_abort = true,
                ConflictResolution::Replace
                    if !self.catalog.indexes_on_table(&table.name).is_empty()
                        || self.replace_needs_row_delete(&table) =>
                {
                    self.multi_write = true
                }
                _ => {}
            }
        }
        let mut unique_touched = table.without_rowid
            && table.columns.iter().any(|c| c.primary_key && is_changed(&c.name));
        for idx in self.catalog.indexes_on_table(&table.name) {
            let touched = rowid_changed
                || idx.columns.iter().any(|ic| match &ic.expr {
                    Some(e) => column_refs(e).iter().any(|r| is_changed(r)),
                    None => is_changed(&ic.name),
                })
                || idx
                    .partial_expr
                    .as_ref()
                    .is_some_and(|p| column_refs(p).iter().any(|r| is_changed(r)));
            if !touched {
                continue;
            }
            for ic in &idx.columns {
                if let Some(e) = &ic.expr {
                    self.scan_expr(e);
                }
            }
            if idx.unique {
                unique_touched = true;
            }
        }
        if unique_touched {
            match mode {
                ConflictResolution::Abort => self.may_abort = true,
                ConflictResolution::Replace if self.replace_needs_row_delete(&table) => {
                    self.multi_write = true
                }
                _ => {}
            }
        }
        if child_fk {
            self.may_abort = true;
        }
        if parent_fks.iter().any(|(_, fk)| {
            !matches!(
                fk.on_update,
                ForeignKeyAction::Cascade | ForeignKeyAction::SetNull
            )
        }) {
            self.may_abort = true;
        }
        self.code_triggers(&triggers, or);
    }

    fn code_delete(&mut self, del: &DeleteStatement, outer_or: Option<ConflictResolution>) {
        if let Some(w) = &del.with {
            self.scan_with(w);
        }
        if let Some(w) = &del.where_clause {
            self.scan_expr(w);
        }
        if let Some(r) = &del.returning {
            self.scan_result_columns(r);
        }
        for t in &del.order_by {
            self.scan_expr(&t.expr);
        }
        if let Some(l) = &del.limit {
            self.scan_expr(l);
        }
        let Some(table) = self.local_table(&del.schema, &del.from) else {
            let trg = self.firing_triggers(&del.from, &|e| matches!(e, TriggerEvent::Delete));
            if trg.is_empty() {
                self.unknown = true;
            } else {
                self.multi_write = true;
                self.code_triggers(&trg, outer_or);
            }
            return;
        };
        let triggers = self.firing_triggers(&table.name, &|e| matches!(e, TriggerEvent::Delete));
        let parent_fks = self.fks_referencing(&table.name);
        let fk_required =
            self.foreign_keys && (!parent_fks.is_empty() || !table.foreign_keys.is_empty());
        let complex = !triggers.is_empty() || fk_required;
        if complex {
            self.multi_write = true;
        }
        // The truncate optimization (no WHERE, nothing complex) codes no
        // per-row loop at all; otherwise only a single-row lookup stays
        // a single write.
        let truncate = del.where_clause.is_none() && !complex && del.limit.is_none();
        if !truncate && !self.where_is_single_row(&table, del.where_clause.as_ref()) {
            self.multi_write = true;
        }
        if parent_fks.iter().any(|(_, fk)| {
            !matches!(
                fk.on_delete,
                ForeignKeyAction::Cascade | ForeignKeyAction::SetNull
            )
        }) {
            self.may_abort = true;
        }
        self.code_triggers(&triggers, outer_or);
    }

    /// sqlite3WhereOkOnePass == ONEPASS_SINGLE: the WHERE pins at most
    /// one row through a rowid equality or an equality on every column of
    /// a (non-partial) UNIQUE index, the compared value not reading the
    /// row itself.
    fn where_is_single_row(&self, table: &Table, where_clause: Option<&Expr>) -> bool {
        let Some(w) = where_clause else {
            return false;
        };
        let mut conjuncts = Vec::new();
        split_and(w, &mut conjuncts);
        let mut eq_cols: Vec<String> = Vec::new();
        for c in conjuncts {
            if let Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
            } = c
            {
                for (col, val) in [(left, right), (right, left)] {
                    if let Expr::Column { name, .. } = col.as_ref() {
                        if column_refs(val).is_empty() && !expr_has_subquery(val) {
                            eq_cols.push(name.to_ascii_lowercase());
                        }
                    }
                }
            }
        }
        let has = |n: &str| eq_cols.iter().any(|c| c.eq_ignore_ascii_case(n));
        if !table.without_rowid
            && (eq_cols.iter().any(|c| is_rowid_spelling(c))
                || table.rowid_alias.is_some_and(|i| has(&table.columns[i].name)))
        {
            return true;
        }
        if table.without_rowid
            && table
                .columns
                .iter()
                .filter(|c| c.primary_key)
                .all(|c| has(&c.name))
        {
            return true;
        }
        self.catalog.indexes_on_table(&table.name).iter().any(|idx| {
            idx.unique
                && idx.partial_expr.is_none()
                && idx.columns.iter().all(|ic| ic.expr.is_none() && has(&ic.name))
        })
    }

    // ── expression scanning: call sites and RAISE(ABORT) ────────────────

    fn scan_expr(&mut self, e: &Expr) {
        if self.may_abort {
            return;
        }
        match e {
            Expr::Literal(_) | Expr::Parameter(_) | Expr::Column { .. } => {}
            Expr::Binary { op, left, right } => {
                if matches!(
                    op,
                    BinaryOp::Arrow | BinaryOp::ArrowText | BinaryOp::FtsMatch | BinaryOp::Distance
                ) {
                    self.may_abort = true;
                }
                self.scan_expr(left);
                self.scan_expr(right);
            }
            Expr::Unary { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::Cast { expr, .. }
            | Expr::Collate { expr, .. } => self.scan_expr(expr),
            Expr::Between {
                expr, low, high, ..
            } => {
                self.scan_expr(expr);
                self.scan_expr(low);
                self.scan_expr(high);
            }
            Expr::In { expr, source, .. } => {
                self.scan_expr(expr);
                match source {
                    InSource::List(list) => {
                        for x in list.iter() {
                            self.scan_expr(x);
                        }
                    }
                    InSource::Subquery(s) => self.scan_select(s),
                    #[allow(unreachable_patterns)]
                    _ => {}
                }
            }
            // LIKE / GLOB / REGEXP / MATCH are function calls in SQLite.
            Expr::Like { .. } => self.may_abort = true,
            Expr::Is { left, right, .. } => {
                self.scan_expr(left);
                self.scan_expr(right);
            }
            Expr::Function {
                name,
                distinct,
                args,
                filter,
                over,
                ..
            } => {
                let lc = name.to_ascii_lowercase();
                let inline = matches!(
                    lc.as_str(),
                    "coalesce" | "ifnull" | "iif" | "if" | "likely" | "unlikely" | "likelihood"
                );
                // Aggregate / window steps are OP_AggStep, not a call site.
                let aggregate = over.is_some()
                    || filter.is_some()
                    || *distinct
                    || is_aggregate_call(&lc, args.len());
                if !inline && !aggregate {
                    self.may_abort = true;
                }
                for a in args {
                    self.scan_expr(a);
                }
                if let Some(f) = filter {
                    self.scan_expr(f);
                }
            }
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                if let Some(o) = operand {
                    self.scan_expr(o);
                }
                for (w, t) in whens {
                    self.scan_expr(w);
                    self.scan_expr(t);
                }
                if let Some(x) = else_ {
                    self.scan_expr(x);
                }
            }
            Expr::Row(items) => {
                for x in items {
                    self.scan_expr(x);
                }
            }
            Expr::Subquery(s) | Expr::Exists(s) => self.scan_select(s),
            Expr::Raise { action, message } => {
                if *action == RaiseAction::Abort {
                    self.may_abort = true;
                }
                if let Some(m) = message {
                    self.scan_expr(m);
                }
            }
        }
    }

    fn scan_with(&mut self, w: &WithClause) {
        for cte in &w.ctes {
            self.scan_select(&cte.select);
        }
    }

    fn scan_select(&mut self, s: &SelectStatement) {
        if let Some(w) = &s.with {
            self.scan_with(w);
        }
        self.scan_body(&s.body);
        for t in &s.order_by {
            self.scan_expr(&t.expr);
        }
        if let Some(l) = &s.limit {
            self.scan_expr(l);
        }
        if let Some(o) = &s.offset {
            self.scan_expr(o);
        }
    }

    fn scan_body(&mut self, b: &SelectBody) {
        match b {
            SelectBody::Simple(s) => {
                self.scan_result_columns(&s.columns);
                if let Some(f) = &s.from {
                    self.scan_from(f);
                }
                if let Some(w) = &s.where_clause {
                    self.scan_expr(w);
                }
                for g in &s.group_by {
                    self.scan_expr(g);
                }
                if let Some(h) = &s.having {
                    self.scan_expr(h);
                }
            }
            SelectBody::Binary { left, right, .. } => {
                self.scan_body(left);
                self.scan_body(right);
            }
        }
    }

    fn scan_result_columns(&mut self, cols: &[ResultColumn]) {
        for c in cols {
            if let ResultColumn::Expr { expr, .. } = c {
                self.scan_expr(expr);
            }
        }
    }

    fn scan_from(&mut self, f: &TableExpression) {
        match f {
            TableExpression::Table { .. } => {}
            TableExpression::Subquery { select, .. } => self.scan_select(select),
            TableExpression::Join {
                left,
                right,
                constraint,
                ..
            } => {
                self.scan_from(left);
                self.scan_from(right);
                if let JoinConstraint::On(e) = constraint {
                    self.scan_expr(e);
                }
            }
            TableExpression::Function { args, .. } => {
                for a in args {
                    self.scan_expr(a);
                }
            }
        }
    }
}

fn is_rowid_spelling(name: &str) -> bool {
    name.eq_ignore_ascii_case("rowid")
        || name.eq_ignore_ascii_case("_rowid_")
        || name.eq_ignore_ascii_case("oid")
}

/// Built-in aggregates by (name, arity) — `max(a)` aggregates, `max(a, b)`
/// is the scalar call.
fn is_aggregate_call(lc: &str, n_args: usize) -> bool {
    match lc {
        "count" | "sum" | "total" | "avg" | "group_concat" | "string_agg"
        | "json_group_array" | "json_group_object" | "jsonb_group_array"
        | "jsonb_group_object" | "median" | "percentile" | "percentile_cont"
        | "percentile_disc" => true,
        "min" | "max" => n_args <= 1,
        _ => false,
    }
}

fn split_and<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match e {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            split_and(left, out);
            split_and(right, out);
        }
        other => out.push(other),
    }
}

fn expr_has_subquery(e: &Expr) -> bool {
    super::expr_has_subquery(e)
}

/// Bare names of the columns an expression reads (subqueries excluded —
/// their columns belong to other scopes).
fn column_refs(e: &Expr) -> Vec<String> {
    fn walk(e: &Expr, out: &mut Vec<String>) {
        match e {
            Expr::Column { name, .. } => out.push(name.clone()),
            Expr::Literal(_) | Expr::Parameter(_) => {}
            Expr::Binary { left, right, .. } | Expr::Is { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            Expr::Unary { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::Cast { expr, .. }
            | Expr::Collate { expr, .. } => walk(expr, out),
            Expr::Between {
                expr, low, high, ..
            } => {
                walk(expr, out);
                walk(low, out);
                walk(high, out);
            }
            Expr::In { expr, source, .. } => {
                walk(expr, out);
                if let InSource::List(list) = source {
                    for x in list.iter() {
                        walk(x, out);
                    }
                }
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                walk(expr, out);
                walk(pattern, out);
                if let Some(x) = escape {
                    walk(x, out);
                }
            }
            Expr::Function { args, .. } => {
                for a in args {
                    walk(a, out);
                }
            }
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                if let Some(o) = operand {
                    walk(o, out);
                }
                for (w, t) in whens {
                    walk(w, out);
                    walk(t, out);
                }
                if let Some(x) = else_ {
                    walk(x, out);
                }
            }
            Expr::Row(items) => {
                for x in items {
                    walk(x, out);
                }
            }
            Expr::Subquery(_) | Expr::Exists(_) => {}
            Expr::Raise { message, .. } => {
                if let Some(m) = message {
                    walk(m, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(e, &mut out);
    out
}
