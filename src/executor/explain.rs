//! EXPLAIN QUERY PLAN rendering — SQLite-compatible row schema
//! (id, parent, notused, detail), used by api::query.
use crate::planner::plan::Plan;
use crate::types::{Row, Value};

/// Render a plan into EXPLAIN QUERY PLAN rows, mirroring SQLite's
/// (id, parent, notused, detail) schema. Children of a node share the
/// parent's id; sibling subtrees are emitted left-to-right.
pub(crate) fn explain_plan_rows(plan: &Plan) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut next_id: i64 = 1;
    walk(plan, 0, &mut rows, &mut next_id);
    rows
}

fn alias_of(alias: &Option<String>, table: &str) -> String {
    alias.clone().unwrap_or_else(|| table.to_string())
}

fn walk(plan: &Plan, parent: i64, rows: &mut Vec<Row>, next_id: &mut i64) {
    match plan {
        Plan::TableFunction { name, args, alias } => {
            let a = alias_of(alias, name);
            push_row(
                parent,
                rows,
                next_id,
                format!(
                    "SCAN {} AS FUNCTION {} ({} argument{})",
                    a,
                    name,
                    args.len(),
                    if args.len() == 1 { "" } else { "s" }
                ),
            );
        }
        Plan::Scan {
            table,
            alias,
            index,
            ..
        } => {
            let a = alias_of(alias, &table.name);
            let detail = match index {
                Some(idx) => format!("SCAN {a} USING INDEX {}", idx.name),
                None => format!("SCAN {a}"),
            };
            push_row(parent, rows, next_id, detail);
        }
        Plan::RowidLookup { table, alias, .. } => {
            let a = alias_of(alias, &table.name);
            push_row(
                parent,
                rows,
                next_id,
                format!("SEARCH {a} USING INTEGER PRIMARY KEY (rowid=?)"),
            );
        }
        Plan::IndexIn {
            table,
            alias,
            index,
            key_exprs,
            ..
        } => {
            let a = alias_of(alias, &table.name);
            push_row(
                parent,
                rows,
                next_id,
                format!(
                    "SEARCH {} USING INDEX {} ({} IN values)",
                    a,
                    index.name,
                    key_exprs.len()
                ),
            );
        }
        Plan::RowidIn {
            table,
            alias,
            values,
            ..
        } => {
            let a = alias_of(alias, &table.name);
            push_row(
                parent,
                rows,
                next_id,
                format!(
                    "SEARCH {} USING INTEGER PRIMARY KEY (rowid IN {} values)",
                    a,
                    values.len()
                ),
            );
        }
        Plan::RowidRange {
            table,
            alias,
            start,
            end,
            ..
        } => {
            let a = alias_of(alias, &table.name);
            let lo = if start.is_some() { "rowid>?" } else { "" };
            let hi = if end.is_some() { "rowid<?" } else { "" };
            let join = if !lo.is_empty() && !hi.is_empty() {
                " AND "
            } else {
                ""
            };
            push_row(
                parent,
                rows,
                next_id,
                format!("SEARCH {a} USING INTEGER PRIMARY KEY ({lo}{join}{hi})"),
            );
        }
        Plan::IndexLookup {
            table,
            alias,
            index,
            key_exprs,
        } => {
            let a = alias_of(alias, &table.name);
            let cols = index_columns_desc(index, key_exprs.len());
            push_row(
                parent,
                rows,
                next_id,
                format!("SEARCH {a} USING INDEX {} ({cols}=?)", index.name),
            );
        }
        Plan::IndexRange {
            table,
            alias,
            index,
            start,
            end,
            ..
        } => {
            let a = alias_of(alias, &table.name);
            let cols = index_columns_desc(index, 1);
            let lo = if start.is_some() {
                format!("{cols}>?")
            } else {
                String::new()
            };
            let hi = if end.is_some() {
                format!("{cols}<?")
            } else {
                String::new()
            };
            let join = if !lo.is_empty() && !hi.is_empty() {
                " AND "
            } else {
                ""
            };
            push_row(
                parent,
                rows,
                next_id,
                format!("SEARCH {a} USING INDEX {} ({lo}{join}{hi})", index.name),
            );
        }
        Plan::Values { .. } => {
            push_row(parent, rows, next_id, "SCAN 1 CONSTANT ROW".to_string());
        }
        Plan::Filter { input, .. } => walk(input, parent, rows, next_id),
        Plan::Project { input, columns } => {
            // Covering-index detection (SQLite's "USING COVERING INDEX"
            // wording): a rowid-only projection over an index access node
            // is answered from the index alone at execution time — see
            // exec_index_lookup_impl / exec_index_range_projected.
            match covering_index_detail(input, columns) {
                Some(detail) => push_row(parent, rows, next_id, detail),
                None => walk(input, parent, rows, next_id),
            }
        }
        Plan::Limit { input, .. } => walk(input, parent, rows, next_id),
        Plan::Distinct { input } => {
            walk(input, parent, rows, next_id);
            push_row(
                parent,
                rows,
                next_id,
                "USE TEMP B-TREE FOR DISTINCT".to_string(),
            );
        }
        Plan::Sort { input, .. } => {
            walk(input, parent, rows, next_id);
            push_row(
                parent,
                rows,
                next_id,
                "USE TEMP B-TREE FOR ORDER BY".to_string(),
            );
        }
        Plan::Aggregate {
            input, group_by, ..
        } => {
            walk(input, parent, rows, next_id);
            if !group_by.is_empty() {
                push_row(
                    parent,
                    rows,
                    next_id,
                    "USE TEMP B-TREE FOR GROUP BY".to_string(),
                );
            }
        }
        Plan::Window { input, .. } => {
            walk(input, parent, rows, next_id);
            push_row(parent, rows, next_id, "USE WINDOW FUNCTION".to_string());
        }
        Plan::Join { left, right, .. } => {
            walk(left, parent, rows, next_id);
            walk(right, parent, rows, next_id);
        }
        Plan::IndexNestedLoopJoin {
            outer,
            inner_table,
            inner_alias,
            inner_index,
            ..
        } => {
            walk(outer, parent, rows, next_id);
            let a = alias_of(inner_alias, &inner_table.name);
            let cols = index_columns_desc(inner_index, 1);
            push_row(
                parent,
                rows,
                next_id,
                format!("SEARCH {a} USING INDEX {} ({cols}=?)", inner_index.name),
            );
        }
        Plan::Subquery { plan } => {
            // Give the subquery its own top-level id group, like SQLite.
            walk(plan, parent, rows, next_id);
        }
        Plan::CteRows { .. } => {
            push_row(parent, rows, next_id, "SCAN CTE".to_string());
        }
        Plan::Union { left, right, .. } => {
            walk(left, parent, rows, next_id);
            walk(right, parent, rows, next_id);
            push_row(
                parent,
                rows,
                next_id,
                "USE TEMP B-TREE FOR UNION".to_string(),
            );
        }
        Plan::Intersect { left, right } => {
            walk(left, parent, rows, next_id);
            walk(right, parent, rows, next_id);
            push_row(
                parent,
                rows,
                next_id,
                "USE TEMP B-TREE FOR INTERSECT".to_string(),
            );
        }
        Plan::Except { left, right } => {
            walk(left, parent, rows, next_id);
            walk(right, parent, rows, next_id);
            push_row(
                parent,
                rows,
                next_id,
                "USE TEMP B-TREE FOR EXCEPT".to_string(),
            );
        }
        Plan::Insert { source, table, .. } => {
            walk(source, parent, rows, next_id);
            let _ = table;
        }
        Plan::Update { source, .. } => walk(source, parent, rows, next_id),
        Plan::Delete { source, .. } => walk(source, parent, rows, next_id),
    }
}

fn index_columns_desc(index: &crate::schema::Index, n: usize) -> String {
    let names: Vec<&str> = index
        .columns
        .iter()
        .take(n.max(1))
        .map(|c| c.name.as_str())
        .collect();
    names.join(",")
}

fn push_row(parent: i64, rows: &mut Vec<Row>, next_id: &mut i64, detail: String) {
    let id = *next_id;
    *next_id += 1;
    rows.push(vec![
        Value::Integer(id),
        Value::Integer(parent),
        Value::Integer(0),
        Value::Text(detail.into()),
    ]);
}

// ============================================================================
// EXPLAIN ANALYZE (PostgreSQL-borrowed)
// ============================================================================

/// One instrumented plan-node execution (recorded by the executor's
/// `execute()` dispatcher while `ctx.explain_stats` is armed).
#[derive(Clone, Debug)]
pub(crate) struct AnalyzeStat {
    /// Node description (same vocabulary as the static EXPLAIN walk).
    pub detail: String,
    /// Dispatch nesting depth (0 = statement root).
    pub depth: usize,
    /// Rows the node emitted.
    pub rows: usize,
    /// Inclusive wall time of the node's whole subtree, seconds.
    pub secs: f64,
}

/// Per-node detail line for EXPLAIN ANALYZE. Mirrors the static walk's
/// vocabulary; wrapper nodes the static rendering passes through get
/// plain names here because ANALYZE shows every node it actually ran.
pub(crate) fn node_detail(plan: &Plan) -> Option<String> {
    match plan {
        Plan::TableFunction { name, args, alias } => {
            let a = alias_of(alias, name);
            Some(format!(
                "SCAN {} AS FUNCTION {} ({} argument{})",
                a,
                name,
                args.len(),
                if args.len() == 1 { "" } else { "s" }
            ))
        }
        Plan::Scan {
            table,
            alias,
            index,
            ..
        } => {
            let a = alias_of(alias, &table.name);
            Some(match index {
                Some(idx) => format!("SCAN {a} USING INDEX {}", idx.name),
                None => format!("SCAN {a}"),
            })
        }
        Plan::RowidLookup { table, alias, .. } => {
            let a = alias_of(alias, &table.name);
            Some(format!("SEARCH {a} USING INTEGER PRIMARY KEY (rowid=?)"))
        }
        Plan::IndexIn {
            table,
            alias,
            index,
            key_exprs,
            ..
        } => {
            let a = alias_of(alias, &table.name);
            Some(format!(
                "SEARCH {} USING INDEX {} ({} IN values)",
                a,
                index.name,
                key_exprs.len()
            ))
        }
        Plan::RowidIn {
            table,
            alias,
            values,
            ..
        } => {
            let a = alias_of(alias, &table.name);
            Some(format!(
                "SEARCH {} USING INTEGER PRIMARY KEY (rowid IN {} values)",
                a,
                values.len()
            ))
        }
        Plan::RowidRange {
            table,
            alias,
            start,
            end,
            ..
        } => {
            let a = alias_of(alias, &table.name);
            let lo = if start.is_some() { "rowid>?" } else { "" };
            let hi = if end.is_some() { "rowid<?" } else { "" };
            let join = if !lo.is_empty() && !hi.is_empty() {
                " AND "
            } else {
                ""
            };
            Some(format!(
                "SEARCH {a} USING INTEGER PRIMARY KEY ({lo}{join}{hi})"
            ))
        }
        Plan::IndexLookup {
            table,
            alias,
            index,
            key_exprs,
        } => {
            let a = alias_of(alias, &table.name);
            let cols = index_columns_desc(index, key_exprs.len());
            Some(format!("SEARCH {a} USING INDEX {} ({cols}=?)", index.name))
        }
        Plan::IndexRange {
            table,
            alias,
            index,
            start,
            end,
            ..
        } => {
            let a = alias_of(alias, &table.name);
            let cols = index_columns_desc(index, 1);
            let lo = if start.is_some() {
                format!("{cols}>?")
            } else {
                String::new()
            };
            let hi = if end.is_some() {
                format!("{cols}<?")
            } else {
                String::new()
            };
            let join = if !lo.is_empty() && !hi.is_empty() {
                " AND "
            } else {
                ""
            };
            Some(format!(
                "SEARCH {a} USING INDEX {} ({lo}{join}{hi})",
                index.name
            ))
        }
        Plan::Values { .. } => Some("SCAN 1 CONSTANT ROW".to_string()),
        Plan::Filter { .. } => Some("FILTER".to_string()),
        Plan::Project { columns, .. } => Some(format!("PROJECT ({} columns)", columns.len())),
        Plan::Limit { .. } => Some("LIMIT".to_string()),
        Plan::Distinct { .. } => Some("USE TEMP B-TREE FOR DISTINCT".to_string()),
        Plan::Sort { .. } => Some("USE TEMP B-TREE FOR ORDER BY".to_string()),
        Plan::Aggregate { group_by, .. } => {
            if group_by.is_empty() {
                Some("AGGREGATE".to_string())
            } else {
                Some("USE TEMP B-TREE FOR GROUP BY".to_string())
            }
        }
        Plan::Window { .. } => Some("USE WINDOW FUNCTION".to_string()),
        Plan::Join {
            join_type,
            algorithm,
            ..
        } => Some(format!("JOIN ({:?}, {:?})", join_type, algorithm)),
        Plan::IndexNestedLoopJoin {
            inner_table,
            inner_alias,
            inner_index,
            ..
        } => {
            let a = alias_of(inner_alias, &inner_table.name);
            let cols = index_columns_desc(inner_index, 1);
            Some(format!(
                "SEARCH {a} USING INDEX {} ({cols}=?) [INLJ]",
                inner_index.name
            ))
        }
        Plan::Subquery { .. } => Some("SUBQUERY".to_string()),
        Plan::CteRows { .. } => Some("SCAN CTE".to_string()),
        Plan::Union { .. } => Some("USE TEMP B-TREE FOR UNION".to_string()),
        Plan::Intersect { .. } => Some("USE TEMP B-TREE FOR INTERSECT".to_string()),
        Plan::Except { .. } => Some("USE TEMP B-TREE FOR EXCEPT".to_string()),
        Plan::Insert { table, .. } => Some(format!("INSERT {}", table.name)),
        Plan::Update { table, .. } => Some(format!("UPDATE {}", table.name)),
        Plan::Delete { table, .. } => Some(format!("DELETE {}", table.name)),
    }
}

/// Render EXPLAIN ANALYZE output rows: (id, depth, actual_rows,
/// elapsed_ms, detail) — one per executed node, in execution (dispatch)
/// order — plus a trailing total row (PostgreSQL's "Execution Time").
pub(crate) fn explain_analyze_rows(stats: &[AnalyzeStat], total_secs: f64) -> Vec<Row> {
    let mut rows = Vec::with_capacity(stats.len() + 1);
    for (i, s) in stats.iter().enumerate() {
        rows.push(vec![
            Value::Integer(i as i64 + 1),
            Value::Integer(s.depth as i64),
            Value::Integer(s.rows as i64),
            Value::Real(s.secs * 1000.0),
            Value::Text(s.detail.clone().into()),
        ]);
    }
    rows.push(vec![
        Value::Integer(stats.len() as i64 + 1),
        Value::Integer(0),
        Value::Integer(stats.len() as i64),
        Value::Real(total_secs * 1000.0),
        Value::Text("Total runtime".into()),
    ]);
    rows
}

/// Column names for the EXPLAIN ANALYZE result set.
pub(crate) fn explain_analyze_columns() -> Vec<String> {
    vec![
        "id".into(),
        "depth".into(),
        "actual_rows".into(),
        "elapsed_ms".into(),
        "detail".into(),
    ]
}

/// True when the projection maps ONLY rowid pseudo-column spellings
/// (rowid/_rowid_/oid) — the shape the executor's covering paths answer
/// straight from index entries.
fn covering_projection(
    columns: &[crate::planner::plan::ProjectExpr],
    table: &crate::schema::Table,
) -> bool {
    match crate::executor::bare_column_projection(columns, table) {
        Some((Some(idxs), _)) => {
            !idxs.is_empty()
                && idxs
                    .iter()
                    .all(|i| *i == crate::storage::row_codec::ROWID_PROJ)
        }
        _ => false,
    }
}

/// SQLite-style detail line for a covering index scan: `SEARCH t USING
/// COVERING INDEX i (...)`. Returns None when the Project input is not a
/// coverable index access (or the projection needs table columns).
fn covering_index_detail(
    input: &Plan,
    columns: &[crate::planner::plan::ProjectExpr],
) -> Option<String> {
    match input {
        Plan::IndexLookup {
            table,
            alias,
            index,
            key_exprs,
        } => {
            if !covering_projection(columns, table) {
                return None;
            }
            let a = alias_of(alias, &table.name);
            let cols = index_columns_desc(index, key_exprs.len());
            Some(format!(
                "SEARCH {a} USING COVERING INDEX {} ({cols}=?)",
                index.name
            ))
        }
        Plan::IndexRange {
            table,
            alias,
            index,
            start,
            end,
            residual,
        } => {
            // A residual re-check needs real column values — the executor
            // only covers the residual-free shape.
            if residual.is_some() || !covering_projection(columns, table) {
                return None;
            }
            let a = alias_of(alias, &table.name);
            let cols = index_columns_desc(index, 1);
            let lo = if start.is_some() {
                format!("{cols}>?")
            } else {
                String::new()
            };
            let hi = if end.is_some() {
                format!("{cols}<?")
            } else {
                String::new()
            };
            let join = if !lo.is_empty() && !hi.is_empty() {
                " AND "
            } else {
                ""
            };
            Some(format!(
                "SEARCH {a} USING COVERING INDEX {} ({lo}{join}{hi})",
                index.name
            ))
        }
        _ => None,
    }
}
