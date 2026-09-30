//! probe_gb_repro — focused repro of the parallel GROUP BY byte-budget
//! spill divergence (tests/parallel_scan.rs's materialized_join_and_collated
//! shape at reduced scale).
use rustqlite::{Database, Value};
use std::time::Instant;

fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(12_000);
    let mut db = Database::open_in_memory().unwrap();
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
                Value::Integer(i * 3),
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO b (id, k, y) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i - (i % 3)),
                Value::Integer(i * 7),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let sql = "SELECT a.k, COUNT(*), SUM(b.y), MIN(a.x), MAX(b.y) FROM a JOIN b ON a.k = b.k GROUP BY a.k";
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let t0 = Instant::now();
    let ser = db.query(sql, []).unwrap();
    let ser_ms = t0.elapsed().as_millis();
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let t1 = Instant::now();
    let par = db.query(sql, []).unwrap();
    let par_ms = t1.elapsed().as_millis();
    println!(
        "n={n} serial: {} rows in {ser_ms}ms | parallel: {} rows in {par_ms}ms",
        ser.len(),
        par.len()
    );
    if ser == par {
        println!("IDENTICAL (order-sensitive)");
    } else {
        println!("DIFFER (order-sensitive) — first divergence:");
        for (i, (s, p)) in ser.iter().zip(par.iter()).enumerate() {
            if s != p {
                println!("  row {i}: ser={s:?} par={p:?}");
                break;
            }
        }
        // sorted-set equality (the real correctness check)
        let mut ss = ser.clone();
        ss.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
        let mut ps = par.clone();
        ps.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
        println!("sorted-set equal: {}", ss == ps);
    }
    // oracle: real SQLite
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER, x INTEGER)",
        [],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER, y INTEGER)",
        [],
    )
    .unwrap();
    for i in 1..=n {
        conn.execute(
            "INSERT INTO a (id, k, x) VALUES (?1, ?2, ?3)",
            rusqlite::params![i, i - (i % 3), i * 3],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO b (id, k, y) VALUES (?1, ?2, ?3)",
            rusqlite::params![i, i - (i % 3), i * 7],
        )
        .unwrap();
    }
    let mut stmt = conn.prepare(sql).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut oracle = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        oracle.push(vec![
            Value::Integer(r.get::<_, i64>(0).unwrap()),
            Value::Integer(r.get::<_, i64>(1).unwrap()),
            Value::Integer(r.get::<_, i64>(2).unwrap()),
            Value::Integer(r.get::<_, i64>(3).unwrap()),
            Value::Integer(r.get::<_, i64>(4).unwrap()),
        ]);
    }
    let mut os = oracle.clone();
    os.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    let mut ss2 = ser.clone();
    ss2.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    println!("serial vs SQLite (sorted): {}", ss2 == os);
}
