use rustqlite::{Database, Value};
fn main() {
    // NULL-bound param over an index equality: SQL says x = NULL matches
    // NOTHING (even rows where x IS NULL).
    for setup in ["plain", "with_null_rows"] {
        let mut db = Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();
        db.execute("CREATE INDEX ix ON t(v)", []).unwrap();
        if setup == "with_null_rows" {
            db.execute("INSERT INTO t VALUES (1, NULL), (2, 10), (3, NULL)", [])
                .unwrap();
        } else {
            db.execute("INSERT INTO t VALUES (1, 10), (2, 20)", [])
                .unwrap();
        }
        let r = db
            .query("SELECT COUNT(*) FROM t WHERE v = ?", vec![Value::Null])
            .unwrap();
        println!("[{}] param=NULL count = {:?} (expect 0)", setup, r[0][0]);
        if setup == "with_null_rows" {
            let r2 = db
                .query("SELECT id FROM t WHERE v = ?", vec![Value::Null])
                .unwrap();
            println!("[{}] param=NULL rows = {:?} (expect empty)", setup, r2);
        }
    }
}
