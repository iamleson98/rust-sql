//! Debug: what plan does BETWEEN produce?
fn main() {
    // Use the planner directly through a query on a real DB and check
    // whether the fast path is taken by timing a cache-hit query vs
    // checking plan structure via a re-plan.
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("INSERT INTO t (name, val, score) VALUES ('a', 1, 1.0)", []).unwrap();

    // Time single-row BETWEEN vs single-row = point lookup:
    let sql_between = "SELECT name, val, score FROM t WHERE id BETWEEN ? AND ?";
    let sql_point = "SELECT name, val, score FROM t WHERE id = ?";
    for _ in 0..100 { let _ = db.query(sql_between, [rustqlite::Value::Integer(1), rustqlite::Value::Integer(1)]).unwrap(); }
    for _ in 0..100 { let _ = db.query(sql_point, [rustqlite::Value::Integer(1)]).unwrap(); }
    let t0 = std::time::Instant::now();
    for _ in 0..1000 { let _ = db.query(sql_between, [rustqlite::Value::Integer(1), rustqlite::Value::Integer(1)]).unwrap(); }
    let d_between = t0.elapsed() / 1000;
    let t0 = std::time::Instant::now();
    for _ in 0..1000 { let _ = db.query(sql_point, [rustqlite::Value::Integer(1)]).unwrap(); }
    let d_point = t0.elapsed() / 1000;
    println!("between 1 row: {:?}   point 1 row: {:?}", d_between, d_point);
}
