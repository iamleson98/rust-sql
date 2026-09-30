//! probe_gb_repro2 — SERIAL scan-path GROUP BY with multi-chunk spills:
//! does the shared-offset reader corruption hit here too?
use rustqlite::{Database, Value};
fn main() {
    let n: i64 = 60_000;
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, v INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        db.execute(
            "INSERT INTO t (id, k, v) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i - (i % 3)),
                Value::Integer(i),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let sql = "SELECT k, COUNT(*), SUM(v) FROM t GROUP BY k";
    let ser = db.query(sql, []).unwrap();
    // oracle
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, v INTEGER)",
        [],
    )
    .unwrap();
    for i in 1..=n {
        conn.execute(
            "INSERT INTO t (id, k, v) VALUES (?1, ?2, ?3)",
            rusqlite::params![i, i - (i % 3), i],
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
        ]);
    }
    let mut ss = ser.clone();
    ss.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    let mut os = oracle.clone();
    os.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    println!(
        "serial scan-path GROUP BY: {} rows vs SQLite {} rows; sorted-equal: {}",
        ser.len(),
        os.len(),
        ss == os
    );
}
