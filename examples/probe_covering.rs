//! Debug probe: plan shape for rowid-only projections over index lookups.
use rustqlite::{Database, Value};

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t(payload TEXT, code TEXT)", [])
        .unwrap();
    for i in 0..50 {
        db.execute(
            "INSERT INTO t VALUES (?, ?)",
            [
                Value::Text(format!("p{}", i).into()),
                Value::Text(format!("c{}", i).into()),
            ],
        )
        .unwrap();
    }
    db.execute("CREATE INDEX icode ON t(code)", []).unwrap();

    for sql in [
        "SELECT rowid FROM t WHERE code = 'c7'",
        "SELECT rowid FROM t WHERE code > 'c5' AND code < 'c9'",
        "SELECT rowid, payload FROM t WHERE code = 'c7'",
    ] {
        let eq = format!("EXPLAIN QUERY PLAN {}", sql);
        let rows = db.query(&eq, []).unwrap();
        println!("SQL: {}", sql);
        for r in &rows {
            println!("  plan: {:?}", r[3].as_text().to_string());
        }
        let got = db.query(sql, []).unwrap();
        println!("  rows: {:?}", got.len());
    }
}
