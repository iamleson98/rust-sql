//! Truth computed in RUST (no query can mask it): does `WHERE a = 3`
//! see all 30 rows when a partial index on a exists?
use rustqlite::{Database, Value};
fn count_a3(db: &mut Database) -> i64 {
    db.query("SELECT COUNT(*) FROM t WHERE a = 3", ()).unwrap()[0][0].as_integer()
}
#[test]
fn partial_index_gating_rust_truth() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
        (),
    )
    .unwrap();
    for i in 1..=500i64 {
        db.execute(
            "INSERT INTO t VALUES (?, ?, ?)",
            vec![
                Value::Integer(i),
                Value::Integer(i % 17),
                Value::Text(format!("s{}", i % 5).into()),
            ],
        )
        .unwrap();
    }
    let truth: i64 = (1..=500).filter(|i| i % 17 == 3).count() as i64;
    assert_eq!(truth, 30);
    // Before any index: full answer.
    assert_eq!(count_a3(&mut db), 30, "no-index baseline");
    db.execute("CREATE INDEX px ON t (a) WHERE id % 2 = 0", ())
        .unwrap();
    assert_eq!(
        count_a3(&mut db),
        30,
        "after partial index: a=3 must see ALL rows"
    );
    // With the sibling indexes too (the variants shape).
    db.execute("CREATE INDEX ex ON t (lower(b))", ()).unwrap();
    db.execute("CREATE INDEX dx ON t (a DESC)", ()).unwrap();
    assert_eq!(
        count_a3(&mut db),
        30,
        "multi-index shape: a=3 must see ALL rows"
    );
}
