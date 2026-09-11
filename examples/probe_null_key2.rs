use rustqlite::{Database, Value};
fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", [])
        .unwrap();
    db.execute("CREATE INDEX ix ON t(v)", []).unwrap();
    db.execute("INSERT INTO t VALUES (1, NULL), (2, 10), (3, NULL)", [])
        .unwrap();
    for (q, note) in [
        ("SELECT COUNT(*) FROM t WHERE v = ?", "eq form"),
        (
            "SELECT COUNT(*) FROM t WHERE v >= ? AND v <= ?",
            "range form",
        ),
        (
            "SELECT COUNT(*) FROM t WHERE v > ? AND v < ?",
            "strict range",
        ),
        ("SELECT COUNT(*) FROM t WHERE v BETWEEN ? AND ?", "between"),
    ] {
        let r = db.query(q, vec![Value::Null, Value::Null]).unwrap();
        println!("{} [{}] -> {:?} (sqlite: 0)", q, note, r[0][0]);
    }
    let r = db
        .query("SELECT COUNT(*) FROM t WHERE v = ?", vec![Value::Null])
        .unwrap();
    println!("single NULL param eq -> {:?} (sqlite: 0)", r[0][0]);
}
