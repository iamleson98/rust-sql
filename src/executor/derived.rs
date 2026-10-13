//! Result-set column types of derived tables — FROM-subqueries, views and
//! CTEs (SQLite's `sqlite3SubqueryColumnTypes`).
//!
//! SQLite gives every derived table a `Table` describing its result set:
//! each column carries the AFFINITY and COLLATION of the leftmost arm's
//! result expression, so `SELECT x = '5' FROM (SELECT i AS x FROM t)`
//! compares an INTEGER column against '5' exactly like `i = '5'` would,
//! and a `c COLLATE NOCASE` column stays NOCASE through a view. The
//! planner builds that table here (from the derived table's plan) and
//! hangs it on the `Plan::Subquery` / `Plan::CteRows` node; the executor
//! registers it in the comparison-affinity registry next to the base
//! tables (`collect_affinity_sources`).

use std::sync::Arc;

use crate::planner::plan::Plan;
use crate::schema::{Column, Table};
use crate::sql::ast::{BinaryOp, Expr, UnaryOp};
use crate::types::{Affinity, Value};

/// A derived table's result-set description.
#[derive(Clone, Debug)]
pub struct DerivedMeta {
    /// Name = the FROM qualifier (alias, else view / CTE name; lowercase).
    /// Columns carry the comparison affinity (`Affinity::None` = none)
    /// and the collation (`"BINARY"` when the body defines none).
    pub table: Arc<Table>,
    /// The columns parallel the derived table's output row positionally
    /// (no star expansion was involved), so positional consumers (join
    /// key affinity checks) may read them.
    pub positional: bool,
}

impl DerivedMeta {
    /// The same result set under another FROM qualifier, optionally with
    /// its columns renamed (`v(a, b)`, `WITH c(a, b)`, `FROM (…) s(a, b)`).
    pub(crate) fn renamed(&self, qualifier: &str, columns: Option<&[String]>) -> Arc<DerivedMeta> {
        let mut cols = self.table.columns.clone();
        if let Some(names) = columns {
            if names.len() != cols.len() {
                return Arc::new(DerivedMeta {
                    table: synthetic_table(qualifier, Vec::new()),
                    positional: false,
                });
            }
            for (c, n) in cols.iter_mut().zip(names) {
                c.name = n.clone();
            }
        }
        Arc::new(DerivedMeta {
            table: synthetic_table(qualifier, cols),
            positional: self.positional,
        })
    }

    /// A description with no known columns — the atom's FROM qualifier
    /// only (a body whose shape is not analysable).
    pub(crate) fn opaque(qualifier: &str) -> Arc<DerivedMeta> {
        Arc::new(DerivedMeta {
            table: synthetic_table(qualifier, Vec::new()),
            positional: false,
        })
    }

    /// Per-output-position affinities, when the columns are positional.
    pub(crate) fn positional_affinities(&self) -> Option<Vec<Affinity>> {
        self.positional
            .then(|| self.table.columns.iter().map(|c| c.affinity).collect())
    }
}

/// One result column of one compound arm, as sqlite3SubqueryColumnTypes
/// reads it.
struct ArmCol {
    name: String,
    /// sqlite3ExprAffinity (`None` = no affinity).
    aff: Option<Affinity>,
    /// sqlite3ExprDataType bitmask (0x01 numeric, 0x02 text, 0x04 blob).
    dtype: u8,
    /// sqlite3ExprCollSeq (`None` = defines none).
    coll: Option<String>,
    /// The expression is a CAST (FLEXNUM when the column is numeric).
    is_cast: bool,
}

/// A column reference's (affinity, collation): see `resolve_column`.
type Resolved = (Option<Affinity>, Option<Option<String>>);

/// A FROM source visible to an arm's result expressions.
struct Source {
    qualifier: String,
    table: Arc<Table>,
}

/// Build the derived-table description of `plan` (a FROM-subquery /
/// view / CTE body) under FROM qualifier `qualifier`. `None` when the
/// body's shape is not analysable (its columns then carry no affinity —
/// the previous behaviour).
pub(crate) fn derived_meta(plan: &Plan, qualifier: &str) -> Option<Arc<DerivedMeta>> {
    let mut arms = Vec::new();
    flatten_arms(plan, &mut arms);
    let mut positional = true;
    let mut cols: Vec<Vec<ArmCol>> = Vec::with_capacity(arms.len());
    for arm in arms {
        cols.push(arm_columns(arm, &mut positional)?);
    }
    let left = cols.first()?;
    let numeric = |a: Affinity| matches!(a, Affinity::Numeric | Affinity::Integer | Affinity::Real);
    let mut out = Vec::with_capacity(left.len());
    for (i, lc) in left.iter().enumerate() {
        let aff_of = |k: usize| cols[k].get(i).and_then(|c| c.aff);
        let dtype_of = |k: usize| cols[k].get(i).map_or(0, |c| c.dtype);
        let mut aff = aff_of(0);
        let mut m = 0u8;
        let mut k = 0usize;
        while aff.is_none() && k + 1 < cols.len() {
            m |= dtype_of(k);
            k += 1;
            aff = aff_of(k);
        }
        let mut aff = aff.unwrap_or(Affinity::None);
        if (matches!(aff, Affinity::Text) || numeric(aff)) && (k + 1 < cols.len() || k != 0) {
            for j in k + 1..cols.len() {
                m |= dtype_of(j);
            }
            if (aff == Affinity::Text && m & 0x01 != 0) || (numeric(aff) && m & 0x02 != 0) {
                aff = Affinity::Blob;
            }
            if numeric(aff) && lc.is_cast {
                aff = Affinity::Numeric; // SQLITE_AFF_FLEXNUM
            }
        }
        out.push(synthetic_column(
            &lc.name,
            aff,
            lc.coll.clone().unwrap_or_else(|| "BINARY".to_string()),
        ));
    }
    Some(Arc::new(DerivedMeta {
        table: synthetic_table(qualifier, out),
        positional,
    }))
}

/// The arms of a (possibly compound) body, leftmost first. A multi-row
/// VALUES is a compound of one-row arms.
fn flatten_arms<'p>(plan: &'p Plan, out: &mut Vec<&'p Plan>) {
    let mut p = plan;
    loop {
        match p {
            Plan::Sort { input, .. } | Plan::Limit { input, .. } | Plan::Distinct { input }
                if is_compound(input) =>
            {
                p = input
            }
            _ => break,
        }
    }
    match p {
        Plan::Union { left, right, .. }
        | Plan::Intersect { left, right }
        | Plan::Except { left, right } => {
            flatten_arms(left, out);
            flatten_arms(right, out);
        }
        _ => out.push(p),
    }
}

fn is_compound(p: &Plan) -> bool {
    match p {
        Plan::Union { .. } | Plan::Intersect { .. } | Plan::Except { .. } => true,
        Plan::Sort { input, .. } | Plan::Limit { input, .. } | Plan::Distinct { input } => {
            is_compound(input)
        }
        _ => false,
    }
}

fn arm_columns(arm: &Plan, positional: &mut bool) -> Option<Vec<ArmCol>> {
    match arm {
        Plan::Distinct { input }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Filter { input, .. } => arm_columns(input, positional),
        Plan::Values { rows } => Some(
            rows.first()?
                .iter()
                .enumerate()
                .map(|(i, e)| ArmCol {
                    name: format!("column{}", i + 1),
                    aff: None,
                    dtype: data_type(e, &|_| None),
                    coll: crate::executor::expr::explicit_collation(e),
                    is_cast: false,
                })
                .collect(),
        ),
        Plan::Project { input, columns } => {
            let mut sources = Vec::new();
            scope_sources(input, &mut sources);
            let resolve = |e: &Expr| resolve_column(e, &sources);
            let mut out = Vec::with_capacity(columns.len());
            for c in columns {
                if let Expr::Column { table, name } = &c.expr {
                    if name == "*" {
                        *positional = false;
                        for s in sources.iter().filter(|s| {
                            table
                                .as_deref()
                                .map_or(true, |q| q.eq_ignore_ascii_case(&s.qualifier))
                        }) {
                            for col in &s.table.columns {
                                out.push(source_column(&s.table, col));
                            }
                        }
                        continue;
                    }
                }
                let name = match (&c.alias, &c.expr) {
                    (Some(a), _) => a
                        .rsplit_once('.')
                        .map_or(a.as_str(), |(_, n)| n)
                        .to_string(),
                    (None, Expr::Column { name, .. }) => name.clone(),
                    // Unnamed expression: never referenced by name here.
                    _ => String::new(),
                };
                let aff = crate::executor::expr::expr_affinity_with(&c.expr, &|e| resolve(e).0)
                    .filter(|a| *a != Affinity::None);
                let coll = crate::executor::expr::explicit_collation(&c.expr)
                    .or_else(|| declared_collation(&c.expr, &resolve));
                out.push(ArmCol {
                    name,
                    aff,
                    dtype: data_type(&c.expr, &|e| resolve(e).0),
                    coll,
                    is_cast: is_plain_cast(&c.expr),
                });
            }
            Some(out)
        }
        Plan::Subquery { meta: Some(m), .. } | Plan::CteRows { meta: Some(m), .. } => {
            *positional &= m.positional;
            Some(
                m.table
                    .columns
                    .iter()
                    .map(|c| source_column(&m.table, c))
                    .collect(),
            )
        }
        _ => None,
    }
}

/// A source column read bare (a star expansion, a pass-through arm).
fn source_column(table: &Table, col: &Column) -> ArmCol {
    let aff = Some(col.affinity).filter(|a| *a != Affinity::None);
    let idx = table.find_column(&col.name);
    ArmCol {
        name: col.name.clone(),
        aff,
        dtype: affinity_data_type(aff),
        coll: if idx.is_some() && idx == table.rowid_alias {
            None
        } else {
            Some(if col.collation.is_empty() {
                "BINARY".to_string()
            } else {
                col.collation.clone()
            })
        },
        is_cast: false,
    }
}

/// The FROM sources of an arm's input, left to right (base tables and
/// derived tables carrying a description).
fn scope_sources(plan: &Plan, out: &mut Vec<Source>) {
    match plan {
        Plan::Scan { table, alias, .. }
        | Plan::RowidLookup { table, alias, .. }
        | Plan::RowidIn { table, alias, .. }
        | Plan::RowidRange { table, alias, .. }
        | Plan::IndexIn { table, alias, .. }
        | Plan::IndexLookup { table, alias, .. }
        | Plan::IndexRange { table, alias, .. }
        | Plan::InvertedIndexScan { table, alias, .. }
        | Plan::SpatialIndexScan { table, alias, .. }
        | Plan::SpatialKnn { table, alias, .. } => out.push(Source {
            qualifier: alias.as_deref().unwrap_or(&table.name).to_ascii_lowercase(),
            table: table.clone(),
        }),
        Plan::Filter { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Aggregate { input, .. }
        | Plan::Window { input, .. }
        | Plan::Distinct { input } => scope_sources(input, out),
        // A projection under the arm's own (ORDER BY / window shapes): its
        // outputs, reachable bare.
        Plan::Project { .. } => {
            if let Some(m) = derived_meta(plan, "") {
                out.push(Source {
                    qualifier: String::new(),
                    table: m.table.clone(),
                });
            }
        }
        Plan::Join { left, right, .. } => {
            scope_sources(left, out);
            scope_sources(right, out);
        }
        Plan::IndexNestedLoopJoin {
            outer,
            inner_table,
            inner_alias,
            ..
        } => {
            scope_sources(outer, out);
            out.push(Source {
                qualifier: inner_alias
                    .as_deref()
                    .unwrap_or(&inner_table.name)
                    .to_ascii_lowercase(),
                table: inner_table.clone(),
            });
        }
        Plan::Subquery { meta: Some(m), .. } | Plan::CteRows { meta: Some(m), .. } => {
            out.push(Source {
                qualifier: m.table.name.clone(),
                table: m.table.clone(),
            })
        }
        _ => {}
    }
}

/// Resolve a column reference against the arm's sources: (affinity,
/// collation-defined) — `.1` is `Some(None)` for the rowid (defines no
/// collation), `None` when the name is not a column of any source.
fn resolve_column(e: &Expr, sources: &[Source]) -> Resolved {
    let Expr::Column { table, name } = e else {
        return (None, None);
    };
    for s in sources {
        if let Some(q) = table {
            if !q.eq_ignore_ascii_case(&s.qualifier) {
                continue;
            }
        }
        if let Some(i) = s.table.find_column(name) {
            let c = &s.table.columns[i];
            let coll = if s.table.rowid_alias == Some(i) {
                None
            } else if c.collation.is_empty() {
                Some("BINARY".to_string())
            } else {
                Some(c.collation.clone())
            };
            return (
                Some(c.affinity).filter(|a| *a != Affinity::None),
                Some(coll),
            );
        }
        if crate::planner::is_rowid_spelling(name) && !s.table.without_rowid {
            return (Some(Affinity::Integer), Some(None));
        }
        if table.is_some() {
            return (None, None);
        }
    }
    (None, None)
}

/// A table column's declared collation seen through CAST / unary `+`
/// (sqlite3ExprCollSeq without an explicit COLLATE).
fn declared_collation(e: &Expr, resolve: &dyn Fn(&Expr) -> Resolved) -> Option<String> {
    let mut p = e;
    loop {
        match p {
            Expr::Cast { expr, .. }
            | Expr::Unary {
                op: UnaryOp::Pos,
                expr,
            } => p = expr,
            Expr::Column { .. } => return resolve(p).1.flatten(),
            _ => return None,
        }
    }
}

fn is_plain_cast(e: &Expr) -> bool {
    matches!(e, Expr::Cast { type_name, .. } if !type_name.starts_with('\0'))
}

fn affinity_data_type(aff: Option<Affinity>) -> u8 {
    match aff {
        Some(Affinity::Numeric | Affinity::Integer | Affinity::Real) => 0x05,
        Some(Affinity::Text) => 0x06,
        _ => 0x07,
    }
}

/// sqlite3ExprDataType: the possible datatypes of an expression's value
/// (0x01 numeric, 0x02 text, 0x04 blob; 0 = always NULL).
fn data_type(e: &Expr, col_aff: &dyn Fn(&Expr) -> Option<Affinity>) -> u8 {
    match e {
        Expr::Collate { expr, .. }
        | Expr::Unary {
            op: UnaryOp::Pos,
            expr,
        } => data_type(expr, col_aff),
        Expr::Literal(Value::Null) => 0x00,
        Expr::Literal(Value::Text(_)) => 0x02,
        Expr::Literal(Value::Blob(_)) => 0x04,
        Expr::Binary {
            op: BinaryOp::Concat,
            ..
        } => 0x06,
        Expr::Parameter(_) | Expr::Function { .. } => 0x07,
        Expr::Column { .. } | Expr::Cast { .. } | Expr::Subquery(_) | Expr::Row(_) => {
            affinity_data_type(
                crate::executor::expr::expr_affinity_with(e, col_aff)
                    .filter(|a| *a != Affinity::None),
            )
        }
        Expr::Case { whens, else_, .. } => {
            let mut r = 0;
            for (_, then) in whens {
                r |= data_type(then, col_aff);
            }
            if let Some(e) = else_ {
                r |= data_type(e, col_aff);
            }
            r
        }
        _ => 0x01,
    }
}

/// A table of named columns carrying only collations (the planner's
/// collation scope for derived tables).
pub(crate) fn collation_table(cols: Vec<(String, String)>) -> Arc<Table> {
    synthetic_table(
        "",
        cols.into_iter()
            .map(|(n, c)| synthetic_column(&n, Affinity::None, c))
            .collect(),
    )
}

fn synthetic_column(name: &str, affinity: Affinity, collation: String) -> Column {
    Column {
        name: name.to_string(),
        affinity,
        declared_type: String::new(),
        decimal: None,
        nullable: true,
        default: None,
        primary_key: false,
        primary_key_order: Default::default(),
        pk_seq: 0,
        explicit_not_null: false,
        autoincrement: false,
        unique: false,
        collation,
        pk_collation: String::new(),
        generated: None,
        not_null_conflict: None,
    }
}

fn synthetic_table(qualifier: &str, columns: Vec<Column>) -> Arc<Table> {
    let name = qualifier.to_ascii_lowercase();
    let col_names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
    let qualified: Vec<String> = col_names.iter().map(|c| format!("{name}.{c}")).collect();
    let affs: Vec<Affinity> = columns.iter().map(|c| c.affinity).collect();
    Arc::new(Table {
        name,
        columns,
        root_page: 0,
        // No rowid: `rowid` through a derived table is not its column.
        without_rowid: true,
        strict: false,
        rowid_alias: None,
        create_sql: String::new(),
        check_exprs: Vec::new(),
        check_labels: Vec::new(),
        foreign_keys: Vec::new(),
        col_names: col_names.into(),
        qualified_col_names: qualified.into(),
        col_affinities: affs.into(),
        vtab: None,
        pk_conflict: None,
        conflict_clauses: false,
    })
}
