//! Cost-searched join-order probe: the shapes the subset DP finds that the
//! old greedy-by-atom-size heuristic could not — engine vs real SQLite,
//! timing + answer parity.
//!
//! 1. Star schema (1M-row fact): a point-filtered dimension drives the
//!    fact table through index probes (INLJ chaining — the DP's order),
//!    instead of hashing the whole fact after the unfiltered dimensions.
//! 2. Bushy pairing (4 tables): two unique-key pairs joined through a
//!    low-distinct link — (A ⋈ B) ⋈ (C ⋈ D) makes the big product the
//!    FINAL output instead of an intermediate a fourth join re-reads.
//!    SQLite is left-deep-only; this is an order SQLite cannot generate.
//! 3. The greedy's blind spot: a tiny table with a NON-selective join vs.
//!    a selective mid-size pair.
//!
//! Set RSQL_DBG_REORDER=1 to watch the DP's decisions (identity cost,
//! best cost, chosen atom order) live.

use rustqlite::{Database, Value};
use std::time::Instant;

fn bench(name: &str, sql: &str, build: &dyn Fn() -> (Database, rusqlite::Connection)) {
    // ---- engine (warm best-of-3; the first run pays plan compile and
    // first-touch cache work on both engines equally) ----
    let (db, conn) = build();
    let _ = db.query(sql, []).unwrap();
    let mut eng_ms = f64::INFINITY;
    let mut rows = Vec::new();
    for _ in 0..3 {
        let t0 = Instant::now();
        rows = db.query(sql, []).unwrap();
        eng_ms = eng_ms.min(t0.elapsed().as_secs_f64() * 1000.0);
    }

    // ---- SQLite (warm best-of-3) ----
    let _ = conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    let mut sq_ms = f64::INFINITY;
    let mut n = 0i64;
    for _ in 0..3 {
        let t0 = Instant::now();
        n = conn.query_row(sql, [], |r| r.get(0)).unwrap();
        sq_ms = sq_ms.min(t0.elapsed().as_secs_f64() * 1000.0);
    }

    let ratio = sq_ms / eng_ms;
    let eng_count = rows
        .first()
        .and_then(|r| r.first())
        .map(|v| v.as_integer())
        .unwrap_or(0);
    println!("{name:<46} rows={n:<10} rustqlite={eng_ms:8.1}ms sqlite={sq_ms:8.1}ms  {ratio:.2}x");
    assert_eq!(eng_count, n, "answer mismatch on {name}");
}

/// Batched multi-row INSERT into both engines identically.
fn fill_both(
    db: &mut Database,
    conn: &rusqlite::Connection,
    table: &str,
    cols: usize,
    rows: i64,
    gen: &dyn Fn(i64) -> String,
) {
    let mut i = 1i64;
    let chunk = 500usize;
    while i <= rows {
        let end = (i + chunk as i64 - 1).min(rows);
        let mut values = Vec::with_capacity(chunk);
        for r in i..=end {
            values.push(format!("({})", gen(r)));
        }
        let colnames: Vec<&str> = (1..=cols)
            .map(|c| match c {
                1 => "c1",
                2 => "c2",
                3 => "c3",
                _ => "c4",
            })
            .collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES {}",
            table,
            colnames.join(", "),
            values.join(", ")
        );
        db.execute(&sql, []).unwrap();
        conn.execute_batch(&sql).unwrap();
        i = end + 1;
    }
}

fn main() {
    println!("== cost-searched join order: engine vs SQLite ==\n");

    // -------------------------------------------------------------------
    // 1. Star schema: 1M-row fact, three dimensions, point filter on dim1.
    // -------------------------------------------------------------------
    {
        let build = || {
            let mut db = Database::open_in_memory().unwrap();
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            let ddl = [
                "CREATE TABLE dim1 (k INTEGER PRIMARY KEY, a TEXT)",
                "CREATE TABLE dim2 (k INTEGER PRIMARY KEY, b TEXT)",
                "CREATE TABLE dim3 (k INTEGER PRIMARY KEY, c TEXT)",
                "CREATE TABLE fact (id INTEGER PRIMARY KEY, c1 INT, c2 INT, c3 INT, c4 INT)",
                "CREATE INDEX idx_fact_d1 ON fact(c1)",
                "CREATE INDEX idx_fact_d2 ON fact(c2)",
                "CREATE INDEX idx_fact_d3 ON fact(c3)",
            ];
            for s in ddl {
                db.execute(s, []).unwrap();
                conn.execute_batch(s).unwrap();
            }
            db.execute("BEGIN", []).unwrap();
            conn.execute_batch("BEGIN").unwrap();
            // fact: d1 ∈ 10, d2 ∈ 20, d3 ∈ 30 distinct, v = id.
            fill_both(&mut db, &conn, "fact", 4, 1_000_000, &|r| {
                format!("{}, {}, {}, {}", r % 10, r % 20, r % 30, r)
            });
            for k in 1..=10i64 {
                let s = format!("INSERT INTO dim1 (k, a) VALUES ({k}, 'a{k}')");
                db.execute(&s, []).unwrap();
                conn.execute_batch(&s).unwrap();
            }
            for k in 1..=20i64 {
                let s = format!("INSERT INTO dim2 (k, b) VALUES ({k}, 'b{k}')");
                db.execute(&s, []).unwrap();
                conn.execute_batch(&s).unwrap();
            }
            for k in 1..=30i64 {
                let s = format!("INSERT INTO dim3 (k, c) VALUES ({k}, 'c{k}')");
                db.execute(&s, []).unwrap();
                conn.execute_batch(&s).unwrap();
            }
            db.execute("COMMIT", []).unwrap();
            conn.execute_batch("COMMIT").unwrap();
            (db, conn)
        };
        let _ = Value::Null;
        bench(
            "star 1M: dim1=3 drives fact by index (INLJ)",
            "SELECT SUM(fact.c4) FROM dim1, dim2, dim3, fact \
             WHERE dim1.k = fact.c1 AND dim2.k = fact.c2 AND dim3.k = fact.c3 \
             AND dim1.k = 3",
            &build,
        );
        bench(
            "star 1M: unfiltered (hash-join economics)",
            "SELECT COUNT(*), SUM(fact.c4) FROM dim1, dim2, dim3, fact \
             WHERE dim1.k = fact.c1 AND dim2.k = fact.c2 AND dim3.k = fact.c3",
            &build,
        );
    }

    // -------------------------------------------------------------------
    // 2. Bushy pairing: two unique-key pairs through a low-distinct link.
    // -------------------------------------------------------------------
    {
        let build = || {
            let mut db = Database::open_in_memory().unwrap();
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            let ddl = [
                "CREATE TABLE ta (id INTEGER PRIMARY KEY, c1 INT, c2 INT)",
                "CREATE TABLE tb (id INTEGER PRIMARY KEY, c1 INT)",
                "CREATE TABLE tc (id INTEGER PRIMARY KEY, c1 INT, c2 INT)",
                "CREATE TABLE td (id INTEGER PRIMARY KEY, c1 INT)",
                "CREATE INDEX idx_tb_y ON tb(c1)",
                "CREATE INDEX idx_td_w ON td(c1)",
            ];
            for s in ddl {
                db.execute(s, []).unwrap();
                conn.execute_batch(s).unwrap();
            }
            db.execute("BEGIN", []).unwrap();
            conn.execute_batch("BEGIN").unwrap();
            // ta.x=tb.y 1:1; tc.z=td.w 1:1; ta.p = tc.q low-distinct (5).
            // Output = n^2/5 rows: 5k tables give 5M output rows — the
            // shape's timing signature without an absurd product.
            let n = 5_000i64;
            fill_both(&mut db, &conn, "ta", 2, n, &|r| format!("{}, {}", r, r % 5));
            fill_both(&mut db, &conn, "tb", 1, n, &|r| format!("{r}"));
            fill_both(&mut db, &conn, "tc", 2, n, &|r| format!("{}, {}", r, r % 5));
            fill_both(&mut db, &conn, "td", 1, n, &|r| format!("{r}"));
            db.execute("COMMIT", []).unwrap();
            conn.execute_batch("COMMIT").unwrap();
            // ANALYZE both sides: with stat1 distincts the DP sees the
            // unique-key pairs and the low-distinct link for what they are.
            db.execute("ANALYZE", []).unwrap();
            conn.execute_batch("ANALYZE").unwrap();
            (db, conn)
        };
        bench(
            "bushy 5k-pairs (ANALYZE): 5M-row output",
            "SELECT COUNT(*) FROM ta, tb, tc, td \
             WHERE ta.c1 = tb.c1 AND tc.c1 = td.c1 AND ta.c2 = tc.c2",
            &build,
        );
    }

    // -------------------------------------------------------------------
    // 3. The greedy's blind spot: tiny non-selective s vs selective pair.
    // -------------------------------------------------------------------
    {
        let build = || {
            let mut db = Database::open_in_memory().unwrap();
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            let ddl = [
                "CREATE TABLE s (id INTEGER PRIMARY KEY, c1 INT)",
                "CREATE TABLE m1 (id INTEGER PRIMARY KEY, c1 INT, c2 INT)",
                "CREATE TABLE m2 (id INTEGER PRIMARY KEY, c1 INT)",
                "CREATE INDEX idx_m2_v ON m2(c1)",
            ];
            for s in ddl {
                db.execute(s, []).unwrap();
                conn.execute_batch(s).unwrap();
            }
            db.execute("BEGIN", []).unwrap();
            conn.execute_batch("BEGIN").unwrap();
            for i in 1..=5i64 {
                let q = format!("INSERT INTO s (c1) VALUES ({i})");
                db.execute(&q, []).unwrap();
                conn.execute_batch(&q).unwrap();
            }
            fill_both(&mut db, &conn, "m1", 2, 100_000, &|r| {
                format!("{}, {}", r % 5, r)
            });
            fill_both(&mut db, &conn, "m2", 1, 100_000, &|r| format!("{r}"));
            db.execute("COMMIT", []).unwrap();
            conn.execute_batch("COMMIT").unwrap();
            (db, conn)
        };
        bench(
            "blind spot 100k: selective pair first",
            "SELECT COUNT(*) FROM s, m1, m2 \
             WHERE s.c1 = m1.c1 AND m1.c2 = m2.c1",
            &build,
        );
    }
}
