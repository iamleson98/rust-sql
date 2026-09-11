use rustqlite::{Database, Value};
fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)", [])
        .unwrap();
    for q in [
        "SELECT COUNT(*) FROM t WHERE id >= ?",
        "SELECT COUNT(*) FROM t WHERE id <= ?",
        "SELECT COUNT(*) FROM t WHERE id = ?",
        "SELECT COUNT(*) FROM t WHERE id > ?",
        "SELECT COUNT(*) FROM t WHERE id BETWEEN ? AND 3",
        "SELECT COUNT(*) FROM t WHERE id IN (?, ?)",
        "SELECT COUNT(*) FROM t WHERE v > ?",
    ] {
        let r = db.query(q, vec![Value::Null]).unwrap();
        println!("{:50} NULL -> {:?} (sqlite: 0)", q, r[0][0]);
    }
}
