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
        Plan::Scan { table, alias, index, .. } => {
            let a = alias_of(alias, &table.name);
            let detail = match index {
                Some(idx) => format!("SCAN {a} USING INDEX {}", idx.name),
                None => format!("SCAN {a}"),
            };
            push_row(parent, rows, next_id, detail);
        }
        Plan::RowidLookup { table, alias, .. } => {
            let a = alias_of(alias, &table.name);
            push_row(parent, rows, next_id, format!("SEARCH {a} USING INTEGER PRIMARY KEY (rowid=?)"));
        }
        Plan::IndexIn { table, alias, index, key_exprs, .. } => {
            let a = alias_of(alias, &table.name);
            push_row(
                parent,
                rows,
                next_id,
                format!("SEARCH {} USING INDEX {} ({} IN values)", a, index.name, key_exprs.len()),
            );
        }
        Plan::RowidIn { table, alias, values, .. } => {
            let a = alias_of(alias, &table.name);
            push_row(
                parent,
                rows,
                next_id,
                format!("SEARCH {} USING INTEGER PRIMARY KEY (rowid IN {} values)", a, values.len()),
            );
        }
        Plan::RowidRange { table, alias, start, end, .. } => {
            let a = alias_of(alias, &table.name);
            let lo = if start.is_some() { "rowid>?" } else { "" };
            let hi = if end.is_some() { "rowid<?" } else { "" };
            let join = if !lo.is_empty() && !hi.is_empty() { " AND " } else { "" };
            push_row(parent, rows, next_id, format!("SEARCH {a} USING INTEGER PRIMARY KEY ({lo}{join}{hi})"));
        }
        Plan::IndexLookup { table, alias, index, key_exprs } => {
            let a = alias_of(alias, &table.name);
            let cols = index_columns_desc(index, key_exprs.len());
            push_row(parent, rows, next_id, format!("SEARCH {a} USING INDEX {} ({cols}=?)", index.name));
        }
        Plan::IndexRange { table, alias, index, start, end, .. } => {
            let a = alias_of(alias, &table.name);
            let cols = index_columns_desc(index, 1);
            let lo = if start.is_some() { format!("{cols}>?") } else { String::new() };
            let hi = if end.is_some() { format!("{cols}<?") } else { String::new() };
            let join = if !lo.is_empty() && !hi.is_empty() { " AND " } else { "" };
            push_row(parent, rows, next_id, format!("SEARCH {a} USING INDEX {} ({lo}{join}{hi})", index.name));
        }
        Plan::Values { .. } => {
            push_row(parent, rows, next_id, "SCAN 1 CONSTANT ROW".to_string());
        }
        Plan::Filter { input, .. } => walk(input, parent, rows, next_id),
        Plan::Project { input, .. } => walk(input, parent, rows, next_id),
        Plan::Limit { input, .. } => walk(input, parent, rows, next_id),
        Plan::Distinct { input } => {
            walk(input, parent, rows, next_id);
            push_row(parent, rows, next_id, "USE TEMP B-TREE FOR DISTINCT".to_string());
        }
        Plan::Sort { input, .. } => {
            walk(input, parent, rows, next_id);
            push_row(parent, rows, next_id, "USE TEMP B-TREE FOR ORDER BY".to_string());
        }
        Plan::Aggregate { input, group_by, .. } => {
            walk(input, parent, rows, next_id);
            if !group_by.is_empty() {
                push_row(parent, rows, next_id, "USE TEMP B-TREE FOR GROUP BY".to_string());
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
        Plan::IndexNestedLoopJoin { outer, inner_table, inner_alias, inner_index, .. } => {
            walk(outer, parent, rows, next_id);
            let a = alias_of(inner_alias, &inner_table.name);
            let cols = index_columns_desc(inner_index, 1);
            push_row(parent, rows, next_id, format!("SEARCH {a} USING INDEX {} ({cols}=?)", inner_index.name));
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
            push_row(parent, rows, next_id, "USE TEMP B-TREE FOR UNION".to_string());
        }
        Plan::Intersect { left, right } => {
            walk(left, parent, rows, next_id);
            walk(right, parent, rows, next_id);
            push_row(parent, rows, next_id, "USE TEMP B-TREE FOR INTERSECT".to_string());
        }
        Plan::Except { left, right } => {
            walk(left, parent, rows, next_id);
            walk(right, parent, rows, next_id);
            push_row(parent, rows, next_id, "USE TEMP B-TREE FOR EXCEPT".to_string());
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
