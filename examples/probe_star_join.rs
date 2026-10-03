//! Star-schema unfiltered join: what plan does the engine pick, and
//! where does the 0.36x go? Isolates the probe_join_dp star shape with
//! EXPLAIN QUERY PLAN + a timing ladder (scan-only → +1 dim → +2 dims
//! → +3 dims) vs SQLite.

use std::time::Instant;

fn build(scale: i64) -> (rustqlite::Database, rusqlite::Connection) {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in [
        "CREATE TABLE dim1 (k INTEGER PRIMARY KEY, a TEXT)",
        "CREATE TABLE dim2 (k INTEGER PRIMARY KEY, b TEXT)",
        "CREATE TABLE dim3 (k INTEGER PRIMARY KEY, c TEXT)",
        "CREATE TABLE fact (id INTEGER PRIMARY KEY, c1 INT, c2 INT, c3 INT, c4 INT)",
    ] {
        db.execute(s, []).unwrap();
        conn.execute_batch(s).unwrap();
    }
    db.execute("BEGIN", []).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    let n = scale;
    let mut i = 0i64;
    while i < n {
        let end = (i + 50_000).min(n);
        let mut vals = String::new();
        for r in i..end {
            if !vals.is_empty() {
                vals.push(',');
            }
            vals.push_str(&format!("({}, {}, {}, {})", r % 10, r % 20, r % 30, r));
        }
        db.execute(
            &format!("INSERT INTO fact (c1, c2, c3, c4) VALUES {vals}"),
            [],
        )
        .unwrap();
        conn.execute_batch(&format!("INSERT INTO fact (c1, c2, c3, c4) VALUES {vals}"))
            .unwrap();
        i = end;
    }
    for (t, cnt) in [("dim1", 10i64), ("dim2", 20), ("dim3", 30)] {
        let col = match t {
            "dim1" => 'a',
            "dim2" => 'b',
            _ => 'c',
        };
        for k in 1..=cnt {
            let s = format!("INSERT INTO {t} (k, {col}) VALUES ({k}, '{col}{k}')");
            db.execute(&s, []).unwrap();
            conn.execute_batch(&s).unwrap();
        }
    }
    db.execute("COMMIT", []).unwrap();
    conn.execute_batch("COMMIT").unwrap();
    (db, conn)
}

fn bench(name: &str, sql: &str, db: &mut rustqlite::Database, conn: &rusqlite::Connection) {
    // warm
    let _ = db.query(sql, []);
    let _ = conn.query_row(sql, [], |_| Ok(()));
    let t0 = Instant::now();
    let r = db.query(sql, []).unwrap();
    let d_r = t0.elapsed();
    let acc = format!(
        "{}",
        r.first()
            .map(|row| row.first().unwrap().clone())
            .unwrap_or(rustqlite::Value::Null)
    );
    let t1 = Instant::now();
    let s: String = conn
        .query_row(sql, [], |row| {
            Ok(format!(
                "{}|{}",
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?
            ))
        })
        .unwrap_or_default();
    let d_s = t1.elapsed();
    println!(
        "{:<58} rq={:>9.1?} sqlite={:>9.1?} ratio={:.2}x  [rq={:.20} sqlite={}]",
        name,
        d_r,
        d_s,
        d_s.as_secs_f64() / d_r.as_secs_f64(),
        acc,
        s
    );
}

fn main() {
    let (mut db, conn) = build(1_000_000);

    // What plan?
    for sql in [
        "SELECT COUNT(*), SUM(fact.c4) FROM dim1, dim2, dim3, fact WHERE dim1.k = fact.c1 AND dim2.k = fact.c2 AND dim3.k = fact.c3",
        "SELECT COUNT(*), SUM(fact.c4) FROM fact, dim1 WHERE dim1.k = fact.c1",
    ] {
        println!("EXPLAIN QUERY PLAN {sql}");
        let rows = db.query(&format!("EXPLAIN QUERY PLAN {sql}"), []).unwrap();
        for r in rows {
            println!("  {:?}", r);
        }
    }
    println!();

    bench(
        "scan only: SUM(c4) FROM fact",
        "SELECT COUNT(*), SUM(c4) FROM fact",
        &mut db,
        &conn,
    );
    bench(
        "+dim1 join",
        "SELECT COUNT(*), SUM(fact.c4) FROM fact, dim1 WHERE dim1.k = fact.c1",
        &mut db,
        &conn,
    );
    bench(
        "+dim1+dim2 join",
        "SELECT COUNT(*), SUM(fact.c4) FROM fact, dim1, dim2 WHERE dim1.k = fact.c1 AND dim2.k = fact.c2",
        &mut db,
        &conn,
    );
    bench(
        "+dim1+dim2+dim3 join (the probe shape)",
        "SELECT COUNT(*), SUM(fact.c4) FROM dim1, dim2, dim3, fact WHERE dim1.k = fact.c1 AND dim2.k = fact.c2 AND dim3.k = fact.c3",
        &mut db,
        &conn,
    );
    // 2-table PK join — the README's 1.18x shape.
    bench(
        "2-table PK join unfiltered",
        "SELECT COUNT(*), SUM(fact.c4) FROM fact, dim2 WHERE dim2.k = fact.c2",
        &mut db,
        &conn,
    );
    // Explicit JOIN ON syntax — does the plan differ from comma+WHERE?
    bench(
        "2-table PK join EXPLICIT ON",
        "SELECT COUNT(*), SUM(fact.c4) FROM fact INNER JOIN dim2 ON dim2.k = fact.c2",
        &mut db,
        &conn,
    );
    // Inverse order (dim first — forced hash build side).
    bench(
        "dims-first order",
        "SELECT COUNT(*), SUM(fact.c4) FROM dim3, dim2, dim1, fact WHERE dim1.k = fact.c1 AND dim2.k = fact.c2 AND dim3.k = fact.c3",
        &mut db,
        &conn,
    );
}
