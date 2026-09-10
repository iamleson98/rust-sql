//! Minimal repro: IndexLookup on a NOCASE-declared column.
use rustqlite::{Database, Value};

fn main() {
    let mut db = Database::open(":memory:").unwrap();
    let _ = db.execute("DROP TABLE IF EXISTS t", ());
    db.execute(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE)",
        (),
    )
    .unwrap();
    for (id, v) in [(1, "apple"), (2, "APPLE"), (3, "banana")] {
        db.execute(
            "INSERT INTO t VALUES (?, ?)",
            vec![Value::Integer(id), Value::Text(v.into())],
        )
        .unwrap();
    }
    for q in [
        "SELECT count(*) FROM t WHERE v = 'APPLE'",
        "SELECT count(*) FROM t WHERE v = 'apple'",
        "SELECT count(*) FROM t WHERE v = 'banana'",
        "SELECT id FROM t WHERE v = 'APPLE' ORDER BY id",
    ] {
        let rows = db.query(q, ()).unwrap();
        println!("{q} => {:?}", rows);
    }
    // WITH an explicit index too
    db.execute("CREATE INDEX ix ON t(v)", ()).unwrap();
    for q in [
        "SELECT count(*) FROM t WHERE v = 'APPLE'",
        "SELECT count(*) FROM t WHERE v = 'apple'",
        "SELECT id FROM t WHERE v = 'apple'",
        "SELECT v FROM t WHERE v = 'APPLE'",
        "SELECT sum(id) FROM t WHERE v = 'APPLE'",
        "SELECT count(*) FROM t WHERE v = 'APPLE' AND id > 0",
        "SELECT count(*) FROM t WHERE v = 'APPLE' GROUP BY v",
    ] {
        let rows = db.query(q, ()).unwrap();
        println!("with index: {q} => {:?}", rows);
    }
    for q in [
        "EXPLAIN SELECT count(*) FROM t WHERE v = 'APPLE'",
        "EXPLAIN SELECT v FROM t WHERE v = 'APPLE'",
        "EXPLAIN SELECT id FROM t WHERE v = 'apple'",
    ] {
        println!("--- {q}");
        for r in &db.query(q, ()).unwrap() {
            println!("  {:?}", r);
        }
    }
}
