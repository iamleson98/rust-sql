//! Measure the query floor: empty maps vs populated maps.

use rustqlite::{Database, Value};

fn main() {
    // Small table: no splits -> root_overrides/max_rowids/index_roots stay EMPTY
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)", [])
        .unwrap();
    for i in 1..=10 {
        db.execute(&format!("INSERT INTO u (name) VALUES ('n{}')", i), [])
            .unwrap();
    }
    let sql = "SELECT name FROM u WHERE id = ?";
    let _ = db.query(sql, [Value::Integer(5)]).unwrap();
    let _ = db.query(sql, [Value::Integer(5)]).unwrap();
    let n = 2000;
    let t = std::time::Instant::now();
    for i in 0..n {
        let r = db.query(sql, [Value::Integer((i % 10) + 1)]).unwrap();
        assert_eq!(r.len(), 1);
    }
    println!(
        "floor, EMPTY maps  : {:>8.2?}",
        t.elapsed().div_f64(n as f64)
    );

    // Same table shape but 61k rows inserted -> maps populated + deeper trees
    let mut db2 = Database::open_in_memory().unwrap();
    db2.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)", [])
        .unwrap();
    db2.execute("BEGIN", []).unwrap();
    for i in 1..=61000 {
        db2.execute(&format!("INSERT INTO u (name) VALUES ('n{}')", i), [])
            .unwrap();
    }
    db2.execute("COMMIT", []).unwrap();
    let _ = db2.query(sql, [Value::Integer(5)]).unwrap();
    let t = std::time::Instant::now();
    for i in 0..n {
        let r = db2.query(sql, [Value::Integer((i % 61000) + 1)]).unwrap();
        assert_eq!(r.len(), 1);
    }
    println!(
        "floor, FULL maps   : {:>8.2?}",
        t.elapsed().div_f64(n as f64)
    );
}
