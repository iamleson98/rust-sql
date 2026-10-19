//! Debug: dump the full plan for a query (argv[1] = SQL).
use rustqlite::api::Database;
use rustqlite::planner::plan::Plan;

fn walk(p: &Plan, s: &mut String, depth: usize) {
    let indent = "  ".repeat(depth);
    let line = match p {
        Plan::Scan {
            table,
            alias,
            predicate,
            index,
            ..
        } => format!(
            "{}Scan {}{} pred={:?} idx={:?}\n",
            indent,
            table.name,
            alias
                .as_ref()
                .map(|a| format!(" AS {a}"))
                .unwrap_or_default(),
            predicate
                .as_ref()
                .map(|e| format!("{e:?}"))
                .unwrap_or("none".into()),
            index
                .as_ref()
                .map(|i| i.name.clone())
                .unwrap_or("none".into()),
        ),
        Plan::IndexLookup { table, .. } => format!("{}IndexLookup {}\n", indent, table.name),
        Plan::RowidLookup { table, .. } => format!("{}RowidLookup {}\n", indent, table.name),
        Plan::RowidRange { table, .. } => format!("{}RowidRange {}\n", indent, table.name),
        Plan::IndexRange { table, .. } => format!("{}IndexRange {}\n", indent, table.name),
        Plan::Join {
            left,
            right,
            join_type,
            condition,
            algorithm,
            ..
        } => {
            let mut x = format!(
                "{}Join {:?} {:?} cond={:?}\n",
                indent,
                join_type,
                algorithm,
                condition
                    .as_ref()
                    .map(|c| format!("{c:?}"))
                    .unwrap_or("none".into())
            );
            walk(left, &mut x, depth + 1);
            walk(right, &mut x, depth + 1);
            x
        }
        Plan::IndexNestedLoopJoin {
            outer,
            inner_table,
            inner_alias,
            inner_index,
            outer_key_col,
            covering,
        } => {
            let mut x = format!(
                "{}INLJ inner={}{} idx={} outer_key_col={} covering={}\n",
                indent,
                inner_table.name,
                inner_alias
                    .as_ref()
                    .map(|a| format!(" AS {a}"))
                    .unwrap_or_default(),
                inner_index.name,
                outer_key_col,
                covering
            );
            walk(outer, &mut x, depth + 1);
            x
        }
        Plan::Filter { input, predicate } => {
            let mut x = format!("{}Filter {:?}\n", indent, predicate);
            walk(input, &mut x, depth + 1);
            x
        }
        Plan::Project { input, columns } => {
            let mut x = format!(
                "{}Project [{}]\n",
                indent,
                columns
                    .iter()
                    .map(|c| format!(
                        "{:?}{}",
                        c.expr,
                        c.alias
                            .as_ref()
                            .map(|a| format!(" AS {a}"))
                            .unwrap_or_default()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            walk(input, &mut x, depth + 1);
            x
        }
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            let mut x = format!(
                "{}Aggregate groups={} aggs={}\n",
                indent,
                group_by.len(),
                aggregates.len()
            );
            walk(input, &mut x, depth + 1);
            x
        }
        Plan::Values { rows } => format!("{}Values({})\n", indent, rows.len()),
        other => format!("{}{:?}\n", indent, std::mem::discriminant(other)),
    };
    s.push_str(&line);
}

fn main() {
    let sql = std::env::args().nth(1).expect("sql");
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE a(c UNIQUE)", []).unwrap();
    db.execute("CREATE TABLE t1(a INTEGER)", []).unwrap();
    db.execute("INSERT INTO a VALUES(4),(0),(9),(-9)", [])
        .unwrap();
    let stmt = rustqlite::sql::parser::parse(&sql).unwrap();
    let plan = rustqlite::api::Database::plan_for_statement_dbg(db.catalog_ref(), &stmt)
        .unwrap()
        .expect("plan");
    let mut s = String::new();
    walk(&plan, &mut s, 0);
    print!("{s}");
    let rows = db.query(&sql, vec![]).unwrap();
    for r in rows {
        println!("ROW {:?}", r);
    }
}
