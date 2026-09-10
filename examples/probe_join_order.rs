//! Join-order probe: adversarial 3-table inner-join chains, engine vs
//! real SQLite, timing + row-count parity. The WHERE point-filter sits
//! on the LAST table of the chain — the syntactic plan computes the
//! big×big intermediate first; the planner's greedy reorder should drive
//! the point-filtered table instead.
//!
//! Also times the classic implicit-join shape (`FROM a, b WHERE a.x=b.x`)
//! whose WHERE conjuncts now FUSE into the join condition (hash join)
//! instead of a cross product + post-filter.

use rustqlite::{Database, Value};
use std::time::Instant;

fn bench(
    name: &str,
    sql: &str,
    setup: &dyn Fn(&mut Database),
    sqlite_setup: &dyn Fn(&rusqlite::Connection),
) {
    // ---- engine ----
    let mut db = Database::open_in_memory().unwrap();
    setup(&mut db);
    let t0 = Instant::now();
    let rows = db.query(sql, []).unwrap();
    let eng_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // ---- SQLite ----
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    sqlite_setup(&conn);
    let t0 = Instant::now();
    let n: i64 = conn.query_row(sql, [], |r| r.get(0)).unwrap();
    let sq_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let ratio = sq_ms / eng_ms;
    let eng_count = rows
        .first()
        .and_then(|r| r.first())
        .map(|v| v.as_integer())
        .unwrap_or(0);
    println!("{name:<44} rows={n:<10} rustqlite={eng_ms:8.1}ms sqlite={sq_ms:8.1}ms  {ratio:.2}x");
    assert_eq!(eng_count, n, "row count mismatch on {name}");
}

fn fill(conn: &rusqlite::Connection, b: usize, m: usize) {
    conn.execute_batch("BEGIN").unwrap();
    for i in 1..=(b as i64) {
        conn.execute("INSERT INTO big1 (k, v) VALUES (?, ?)", (i % 500, i % 7))
            .unwrap();
        conn.execute("INSERT INTO big2 (k, w) VALUES (?, ?)", (i % 500, i % 11))
            .unwrap();
        if i <= m as i64 {
            conn.execute("INSERT INTO small (k, s) VALUES (?, ?)", (i % 500, i))
                .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
}

fn main() {
    let ddl = "CREATE TABLE big1 (id INTEGER PRIMARY KEY, k INTEGER, v INT), \
               CREATE TABLE big2 (id INTEGER PRIMARY KEY, k INTEGER, w INT), \
               CREATE TABLE small (id INTEGER PRIMARY KEY, k INTEGER, s INT)";
    let _ = ddl;

    for &(b, m, filt) in &[
        (50_000usize, 5usize, "small.id = 3"),
        (100_000, 5, "small.id = 3"),
    ] {
        // ---- engine setup ----
        let eng_setup = move |db: &mut Database| {
            db.execute(
                "CREATE TABLE big1 (id INTEGER PRIMARY KEY, k INTEGER, v INT)",
                [],
            )
            .unwrap();
            db.execute(
                "CREATE TABLE big2 (id INTEGER PRIMARY KEY, k INTEGER, w INT)",
                [],
            )
            .unwrap();
            db.execute(
                "CREATE TABLE small (id INTEGER PRIMARY KEY, k INTEGER, s INT)",
                [],
            )
            .unwrap();
            db.execute("CREATE INDEX i1 ON big1(k)", []).unwrap();
            db.execute("CREATE INDEX i2 ON big2(k)", []).unwrap();
            db.execute("BEGIN", []).unwrap();
            for i in 1..=(b as i64) {
                db.execute(
                    "INSERT INTO big1 (k, v) VALUES (?, ?)",
                    [Value::Integer(i % 500), Value::Integer(i % 7)],
                )
                .unwrap();
                db.execute(
                    "INSERT INTO big2 (k, w) VALUES (?, ?)",
                    [Value::Integer(i % 500), Value::Integer(i % 11)],
                )
                .unwrap();
                if i <= m as i64 {
                    db.execute(
                        "INSERT INTO small (k, s) VALUES (?, ?)",
                        [Value::Integer(i % 500), Value::Integer(i)],
                    )
                    .unwrap();
                }
            }
            db.execute("COMMIT", []).unwrap();
            db.execute("ANALYZE", []).unwrap();
        };
        let sq_setup = move |c: &rusqlite::Connection| {
            c.execute_batch(
                "CREATE TABLE big1 (id INTEGER PRIMARY KEY, k INTEGER, v INT);
CREATE TABLE big2 (id INTEGER PRIMARY KEY, k INTEGER, w INT);
CREATE TABLE small (id INTEGER PRIMARY KEY, k INTEGER, s INT);
CREATE INDEX i1 ON big1(k); CREATE INDEX i2 ON big2(k);",
            )
            .unwrap();
            fill(c, b, m);
            c.execute_batch("ANALYZE").unwrap();
        };

        let q = format!(
            "SELECT COUNT(*) FROM big1 JOIN big2 ON big1.k = big2.k \
             JOIN small ON small.k = big1.k WHERE {filt}"
        );
        bench(
            &format!("adversarial 3-join ({}x{}x{})", b, b, m),
            &q,
            &eng_setup,
            &sq_setup,
        );

        // Same data, benign order (small FIRST) — the shape both planners
        // should already handle.
        let q2 = format!(
            "SELECT COUNT(*) FROM small JOIN big1 ON small.k = big1.k \
             JOIN big2 ON big1.k = big2.k WHERE {filt}"
        );
        bench(
            &format!("benign 3-join ({}x{}x{})", m, b, b),
            &q2,
            &eng_setup,
            &sq_setup,
        );
    }

    // ---- implicit 2-table join: WHERE fusion vs cross+filter ----
    {
        let eng_setup = |db: &mut Database| {
            db.execute(
                "CREATE TABLE f1 (id INTEGER PRIMARY KEY, x INT, t TEXT)",
                [],
            )
            .unwrap();
            db.execute(
                "CREATE TABLE f2 (id INTEGER PRIMARY KEY, x INT, u TEXT)",
                [],
            )
            .unwrap();
            db.execute("BEGIN", []).unwrap();
            for i in 1..=50_000i64 {
                db.execute(
                    "INSERT INTO f1 (x, t) VALUES (?, ?)",
                    [Value::Integer(i % 300), Value::Text("t".into())],
                )
                .unwrap();
                db.execute(
                    "INSERT INTO f2 (x, u) VALUES (?, ?)",
                    [Value::Integer(i % 300), Value::Text("u".into())],
                )
                .unwrap();
            }
            db.execute("COMMIT", []).unwrap();
        };
        let sq_setup = |c: &rusqlite::Connection| {
            c.execute_batch(
                "CREATE TABLE f1 (id INTEGER PRIMARY KEY, x INT, t TEXT);
CREATE TABLE f2 (id INTEGER PRIMARY KEY, x INT, u TEXT); BEGIN;",
            )
            .unwrap();
            for i in 1..=50_000i64 {
                c.execute("INSERT INTO f1 (x, t) VALUES (?, 't')", (i % 300,))
                    .unwrap();
                c.execute("INSERT INTO f2 (x, u) VALUES (?, 'u')", (i % 300,))
                    .unwrap();
            }
            c.execute_batch("COMMIT").unwrap();
        };
        bench(
            "implicit 2-join (FROM a,b WHERE a.x=b.x) 50k",
            "SELECT COUNT(*) FROM f1, f2 WHERE f1.x = f2.x",
            &eng_setup,
            &sq_setup,
        );
        bench(
            "implicit 2-join (benign: JOIN ON) 50k",
            "SELECT COUNT(*) FROM f1 JOIN f2 ON f1.x = f2.x",
            &eng_setup,
            &sq_setup,
        );
    }
}
