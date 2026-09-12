//! Predicate-pushdown gap probe: WHERE conjuncts that could evaluate
//! inside a scan/subquery/CTE/compound body but currently sit as a top
//! Filter over materialized output. Engine EXPLAIN vs SQLite EXPLAIN.

use rustqlite::Database;

fn explain(db: &Database, q: &str) -> Vec<String> {
    db.query(&format!("EXPLAIN QUERY PLAN {q}"), [])
        .unwrap()
        .iter()
        .map(|row| {
            row.iter()
                .filter_map(|v| {
                    let t = v.as_text();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn shape(name: &str, setup: &str, q: &str) {
    let mut db = Database::open_in_memory().unwrap();
    for stmt in setup.split(';') {
        let stmt = stmt.trim();
        if !stmt.is_empty() {
            db.execute(stmt, []).unwrap();
        }
    }
    let eng = explain(&db, q);
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(setup).unwrap();
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {q}")).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut sq = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        // SQLite's EQP: id, parent, notused, detail
        sq.push(r.get::<_, String>(3).unwrap());
    }
    println!("== {name} ==");
    for l in &eng {
        println!("  eng: {l}");
    }
    for l in &sq {
        println!("  sq:  {l}");
    }
    println!();
}

fn main() {
    let base = "CREATE TABLE t (id INTEGER PRIMARY KEY, k INT, v INT);
                CREATE INDEX idx_t_k ON t(k);
                INSERT INTO t (k, v) VALUES (1,10),(2,20),(3,30),(4,40),(5,50);
                CREATE TABLE u (id INTEGER PRIMARY KEY, j INT, w INT);
                INSERT INTO u (j, w) VALUES (1,100),(2,200),(3,300);";

    shape(
        "filter over subquery with projection",
        base,
        "SELECT * FROM (SELECT k, v FROM t) AS s WHERE s.k > 3",
    );
    shape(
        "filter over subquery SELECT *",
        base,
        "SELECT * FROM (SELECT * FROM t) AS s WHERE s.k > 3",
    );
    shape(
        "filter over UNION ALL compound",
        base,
        "SELECT * FROM (SELECT k, v FROM t UNION ALL SELECT j, w FROM u) AS c WHERE c.k > 3",
    );
    shape(
        "filter over UNION compound",
        base,
        "SELECT * FROM (SELECT k, v FROM t UNION SELECT j, w FROM u) AS c WHERE c.k > 2",
    );
    shape(
        "filter over CTE",
        base,
        "WITH c AS (SELECT k, v FROM t) SELECT * FROM c WHERE k > 3",
    );
    shape(
        "filter over CTE used twice",
        base,
        "WITH c AS (SELECT k, v FROM t) SELECT * FROM c a, c b WHERE a.k = b.k AND a.k > 3",
    );
    shape(
        "join conjunct over subquery side",
        base,
        "SELECT * FROM (SELECT k FROM t) s JOIN u ON s.k = u.j",
    );
    shape(
        "filter over view",
        base,
        "CREATE VIEW vv AS SELECT k, v FROM t;
         SELECT * FROM vv WHERE k > 3",
    );
    shape(
        "aggregated subquery + outer filter on group key",
        base,
        "SELECT * FROM (SELECT k, COUNT(*) c FROM t GROUP BY k) g WHERE g.k > 3",
    );
    shape(
        "filter over LEFT JOIN output",
        base,
        "SELECT * FROM t LEFT JOIN u ON t.k = u.j WHERE t.v > 25",
    );
}
