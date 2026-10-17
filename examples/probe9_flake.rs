//! Local probe for the macOS semantics-battery flake: which plan shape
//! does the compound-derived-table statement take, and what does it
//! return? Run repeatedly to watch for flapping.
use rustqlite::{Database, Value};

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    for i in 0..n {
        let mut db = Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE m(i INTEGER, t TEXT, c TEXT COLLATE NOCASE, r REAL, b BLOB, n NUMERIC, u)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO m VALUES (5, '5', 'a', 5.0, x'35', '5', '5')",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO m VALUES (7, 'A', 'B', 7.5, '7', 7, 7)", [])
            .unwrap();

        let sql = "SELECT x FROM (SELECT i AS x FROM m UNION ALL SELECT t FROM m) WHERE x = '5'";
        // The plan shape: does the compound derived table become a
        // Subquery node WITH meta (typed columns -> affinity applies)
        // or a bare Union (no affinity source)?
        let plan = db.query(&format!("EXPLAIN QUERY PLAN {sql}"), []).unwrap();
        for row in &plan {
            let detail = row
                .iter()
                .map(|v| match v {
                    Value::Text(t) => t.to_string(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join(" | ");
            println!("  plan: {detail}");
        }
        let rows = db.query(sql, []).unwrap();
        let rendered: Vec<String> = rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| match v {
                        Value::Integer(i) => format!("Int({i})"),
                        Value::Text(t) => format!("Text({})", t.as_str()),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect();
        println!("run {i}: rows = [{}]", rendered.join("; "));
        // Also the mirror statement and the previous flake's statement.
        let sql2 = "SELECT x FROM (SELECT i AS x FROM m) WHERE x = '5'";
        let rows2 = db.query(sql2, []).unwrap();
        println!("  simple-derived rows = {}", rows2.len());
    }
}
