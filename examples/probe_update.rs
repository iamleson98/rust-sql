//! Probe UPDATE range path: which plan, where does time go.

use rustqlite::Database;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000 {
        db.execute(&format!("INSERT INTO t (name, val, score) VALUES ('n{}', {}, {})", i, i, i as f64 * 1.5), []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    // Reset scores so the update does identical work each round.
    let n = 20;

    // 1. Index range SELECT COUNT (planner uses IndexRange?)
    let sql_count = "SELECT COUNT(*) FROM t WHERE val > 5000";
    let _ = db.query(sql_count, []).unwrap();
    let t = std::time::Instant::now();
    for _ in 0..n {
        let r = db.query(sql_count, []).unwrap();
        assert_eq!(r[0][0], rustqlite::Value::Integer(5000));
    }
    println!("COUNT via range     : {:>9.2?}", t.elapsed().div_f64(n as f64));

    // 2. UPDATE range
    let sql_u = "UPDATE t SET score = score + 1.0 WHERE val > 5000";
    let _ = db.execute(sql_u, []).unwrap();
    let t = std::time::Instant::now();
    for _ in 0..n {
        db.execute(sql_u, []).unwrap();
    }
    println!("UPDATE range        : {:>9.2?}", t.elapsed().div_f64(n as f64));

    // 3. UPDATE by full-table scan with filter (no index): SET score WHERE val > 5000
    //    (drop index to force scan — separate DB to keep idx for #2)
    let mut db2 = Database::open_in_memory().unwrap();
    db2.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db2.execute("BEGIN", []).unwrap();
    for i in 1..=10000 {
        db2.execute(&format!("INSERT INTO t (name, val, score) VALUES ('n{}', {}, {})", i, i, i as f64 * 1.5), []).unwrap();
    }
    db2.execute("COMMIT", []).unwrap();
    let t = std::time::Instant::now();
    for _ in 0..n {
        db2.execute(sql_u, []).unwrap();
    }
    println!("UPDATE range NO idx : {:>9.2?}", t.elapsed().div_f64(n as f64));
}
