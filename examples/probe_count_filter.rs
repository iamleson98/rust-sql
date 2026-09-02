//! Profile filtered COUNT: is the fused aggregate path being used?
use std::time::Instant;

fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        let sql = format!(
            "INSERT INTO t (name, val, score) VALUES ('user{}', {}, {})",
            i, i, i as f64
        );
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // Warm
    for _ in 0..3 {
        let _ = db
            .query("SELECT COUNT(*) FROM t WHERE val > 5000", [])
            .unwrap();
    }
    let start = Instant::now();
    for _ in 0..10 {
        let r = db
            .query("SELECT COUNT(*) FROM t WHERE val > 5000", [])
            .unwrap();
        assert_eq!(r[0][0].as_integer(), 5000);
    }
    println!(
        "COUNT(*) WHERE val>5000 x10: {:?} ({:?}/query)",
        start.elapsed(),
        start.elapsed() / 10
    );

    // Unfiltered COUNT
    let start = Instant::now();
    for _ in 0..10 {
        let r = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(r[0][0].as_integer(), 10000);
    }
    println!(
        "COUNT(*) x10: {:?} ({:?}/query)",
        start.elapsed(),
        start.elapsed() / 10
    );

    // Aggregate no filter
    let start = Instant::now();
    for _ in 0..10 {
        let _ = db
            .query("SELECT SUM(val), AVG(score), MIN(val), MAX(val) FROM t", [])
            .unwrap();
    }
    println!(
        "SUM/AVG/MIN/MAX x10: {:?} ({:?}/query)",
        start.elapsed(),
        start.elapsed() / 10
    );

    // SQLite for reference
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
    )
    .unwrap();
    for i in 1..=10_000i64 {
        let sql = format!(
            "INSERT INTO t (name, val, score) VALUES ('user{}', {}, {})",
            i, i, i as f64
        );
        conn.execute(&sql, []).unwrap();
    }
    let mut stmt = conn
        .prepare("SELECT COUNT(*) FROM t WHERE val > 5000")
        .unwrap();
    let start = Instant::now();
    for _ in 0..10 {
        let c: i64 = stmt.query_row([], |r| r.get(0)).unwrap();
        assert_eq!(c, 5000);
    }
    println!(
        "sqlite COUNT(*) WHERE x10: {:?} ({:?}/query)",
        start.elapsed(),
        start.elapsed() / 10
    );
}
