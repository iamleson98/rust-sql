fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, big TEXT)", []).unwrap();
    let s = "x".repeat(20_000);
    let r = db.execute("INSERT INTO t (big) VALUES (?)", [rustqlite::Value::Text(s.clone().into())]);
    println!("20KB insert: {:?}", r.is_ok());
    let rows = db.query("SELECT LENGTH(big) FROM t", []).unwrap();
    println!("length: {:?}", rows.len());
    let s2 = "y".repeat(100_000);
    let r2 = db.execute("INSERT INTO t (big) VALUES (?)", [rustqlite::Value::Text(s2.clone().into())]);
    println!("100KB insert: {:?}", r2.is_ok());
    let rows2 = db.query("SELECT LENGTH(big) FROM t", []).unwrap();
    println!("rows: {}", rows2.len());
}
