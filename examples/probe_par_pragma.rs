//! Probe: how does `PRAGMA parallel_scan=ON/1/OFF` land?
use rustqlite::{Database, Value};

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    for sql in [
        "PRAGMA parallel_scan=ON",
        "PRAGMA parallel_scan=on",
        "PRAGMA parallel_scan=1",
        "PRAGMA parallel_scan=TRUE",
        "PRAGMA parallel_scan=OFF",
    ] {
        let r = db.execute(sql, []);
        let val = db
            .query("PRAGMA parallel_scan", [])
            .map(|rows| rows[0][0].clone())
            .unwrap_or(Value::Null);
        println!("{:32} -> exec={:?} read={:?}", sql, r, val);
    }
    let _ = Value::Integer(0);
}
