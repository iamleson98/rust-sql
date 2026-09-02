use rustqlite::{Database, Value};

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB, s TEXT, v INTEGER)", []).unwrap();
    for i in 0..12i64 {
        db.execute(
            "INSERT INTO t (b, s, v) VALUES (?, ?, ?)",
            [
                Value::Blob(vec![(i % 256) as u8; (i as usize % 9000) + 1]),
                Value::Text(format!("row-{i}-{}", "y".repeat((i as usize % 7000) + 1)).into()),
                Value::Integer(i),
            ],
        ).unwrap();
    }
    let all = db.query("SELECT id, length(b), v FROM t ORDER BY id", []).unwrap();
    for r in &all { println!("scan: {:?}", r); }
    let one = db.query("SELECT length(b) FROM t WHERE id = 5", []).unwrap();
    println!("literal id=5: {:?}", one);
    let one = db.query("SELECT length(b) FROM t WHERE id = ?", [Value::Integer(5)]).unwrap();
    println!("bind id=5: {:?}", one);
    let one = db.query("SELECT v FROM t WHERE id = ?", [Value::Integer(5)]).unwrap();
    println!("bind v id=5: {:?}", one);
}
