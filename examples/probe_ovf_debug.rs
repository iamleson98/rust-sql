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
    for i in 0..12i64 {
        let rows = db.query("SELECT length(b) FROM t WHERE id = ?", [Value::Integer(i)]).unwrap();
        let rows2 = db.query("SELECT v FROM t WHERE id = ?", [Value::Integer(i)]).unwrap();
        println!("id={i}: length(b)={:?} v={:?}", rows[0][0], rows2[0][0]);
    }
}
