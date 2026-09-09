//! Parallel join probe: 1M x 1M equi-join, serial vs parallel vs SQLite.
use rustqlite::Value;
use std::time::Instant;

fn build_rq(n: i64) -> rustqlite::Database {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER, x INTEGER)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER, y INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        db.execute(
            "INSERT INTO a (id, k, x) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i - (i % 3)),
                Value::Integer(i),
            ],
        )
        .unwrap();
    }
    for i in 1..=n {
        db.execute(
            "INSERT INTO b (id, k, y) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i - (i % 3)),
                Value::Integer(-i),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn main() {
    let n = 1_000_000i64;
    let q = "SELECT COUNT(*), SUM(a.x) FROM a JOIN b ON b.k = a.k";
    let mut db = build_rq(n);
    // Warm (join build cache + pages).
    let _ = db.query(q, []).unwrap();
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let t = Instant::now();
    let ser = db.query(q, []).unwrap();
    let d_ser = t.elapsed();
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let t = Instant::now();
    let par = db.query(q, []).unwrap();
    let d_par = t.elapsed();
    assert_eq!(ser, par, "parallel != serial");
    println!(
        "serial   : {:>8.1} ms  {:?}",
        d_ser.as_secs_f64() * 1000.0,
        ser[0]
    );
    println!(
        "parallel : {:>8.1} ms  ({:.2}x vs serial)",
        d_par.as_secs_f64() * 1000.0,
        d_ser.as_secs_f64() / d_par.as_secs_f64()
    );

    // SQLite on the same shape.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER, x INTEGER); CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER, y INTEGER);").unwrap();
    conn.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        conn.execute(
            "INSERT INTO a (id, k, x) VALUES (?, ?, ?)",
            rusqlite::params![i, i - (i % 3), i],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO b (id, k, y) VALUES (?, ?, ?)",
            rusqlite::params![i, i - (i % 3), -i],
        )
        .unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    conn.query_row(q, [], |_| Ok(())).unwrap();
    let t = Instant::now();
    let (cnt, sum): (i64, i64) = conn
        .query_row(q, [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    let d_sq = t.elapsed();
    println!(
        "sqlite   : {:>8.1} ms  (parallel {:.2}x vs sqlite)",
        d_sq.as_secs_f64() * 1000.0,
        d_sq.as_secs_f64() / d_par.as_secs_f64()
    );
    assert_eq!(cnt, ser[0][0].as_integer());
    assert_eq!(sum, ser[0][1].as_integer());
}
