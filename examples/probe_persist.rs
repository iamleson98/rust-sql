fn main() {
    let path = "/tmp/probe_persist.db";
    let _ = std::fs::remove_file(path);
    {
        let mut db = rustqlite::Database::open(path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", []).unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=10_000i64 {
            db.execute("INSERT INTO t (val) VALUES (?)", [rustqlite::Value::Integer(i)]).unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        let c = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        println!("before close: count = {:?} (expect 10000)", c[0][0]);
    }
    // Reopen.
    let db2 = rustqlite::Database::open(path).unwrap();
    let c2 = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
    println!("after reopen: count = {:?} (expect 10000)", c2[0][0]);
    let r = db2.query("SELECT id, val FROM t WHERE id = 5000", []).unwrap();
    println!("after reopen: row 5000 = {:?} (expect present)", if r.is_empty() { "MISSING".to_string() } else { format!("{:?}", r[0]) });
    let r2 = db2.query("SELECT MAX(val) FROM t", []).unwrap();
    println!("after reopen: MAX(val) = {:?} (expect 10000)", r2[0][0]);
}
