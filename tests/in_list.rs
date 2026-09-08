use rustqlite::{Database, Value};

#[test]
fn in_list_semantics_and_planning() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, k INTEGER)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_k ON t(k)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1000i64 {
        let sql = format!("INSERT INTO t (v, k) VALUES ('v{}', {})", i, i % 10);
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // Rowid IN: correct rows, in rowid order.
    let rows = db
        .query("SELECT v FROM t WHERE id IN (5, 3, 9, 100, 3)", [])
        .unwrap();
    let got: Vec<String> = rows.iter().map(|r| r[0].as_text()).collect();
    assert_eq!(got, vec!["v3", "v5", "v9", "v100"], "rowid IN: {:?}", got);

    // Rowid IN with params.
    let rows = db
        .query(
            "SELECT id FROM t WHERE id IN (?, ?, ?) ORDER BY id",
            [Value::Integer(7), Value::Integer(2), Value::Integer(7)],
        )
        .unwrap();
    assert_eq!(rows.len(), 2);

    // Rowid IN with non-integer + NULL members.
    let rows = db
        .query("SELECT id FROM t WHERE id IN (1, 'x', NULL, 2.5, 3)", [])
        .unwrap();
    assert_eq!(rows.len(), 2, "non-int members skipped: {:?}", rows);

    // Rowid IN with residual.
    let rows = db
        .query(
            "SELECT id FROM t WHERE id IN (1,2,3,4,5,6) AND v = 'v4'",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(4));

    // NOT IN keeps scan semantics (all minus listed).
    let rows = db
        .query("SELECT COUNT(*) FROM t WHERE id NOT IN (1,2,3)", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(997));

    // Empty IN: either a graceful parse error or zero rows — never a panic.
    let _ = db.query("SELECT id FROM t WHERE id IN ()", []);

    // Index IN: k IN (1, 3, 5) -> 100 rows per key % 10.
    let rows = db
        .query("SELECT COUNT(*) FROM t WHERE k IN (1, 3, 5)", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(300), "index IN count");
    // In rowid order check: fetch ids.
    let rows = db.query("SELECT id FROM t WHERE k IN (0)", []).unwrap();
    assert_eq!(rows.len(), 100);
    assert_eq!(rows[0][0], Value::Integer(10));

    // Index IN with residual.
    let rows = db
        .query("SELECT COUNT(*) FROM t WHERE k IN (2, 4) AND id > 500", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(100), "residual count");

    // Index IN with params.
    let rows = db
        .query(
            "SELECT COUNT(*) FROM t WHERE k IN (?, ?)",
            [Value::Integer(1), Value::Integer(2)],
        )
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(200));

    // UPDATE with rowid IN.
    db.execute("UPDATE t SET v = 'upd' WHERE id IN (10, 20, 30)", [])
        .unwrap();
    let rows = db
        .query("SELECT COUNT(*) FROM t WHERE v = 'upd'", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(3));

    // DELETE with index IN.
    db.execute("DELETE FROM t WHERE k IN (9)", []).unwrap();
    // 1000 - 100 (k=9 rows); the 3 updated rows have k=0, unaffected.
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(900));

    // EXPLAIN shape works.
    let _ = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM t WHERE id IN (1, 2)", [])
        .unwrap();
    let _ = db
        .query("EXPLAIN QUERY PLAN SELECT id FROM t WHERE k IN (1, 2)", [])
        .unwrap();

    println!("IN-list semantics: all assertions passed");
}

/// The torture-S11 shape: a 5000-literal IN list through the statement
/// cache — with the Arc-shared list, re-executions must stay correct
/// (the plan's list and the AST's are ONE object now).
#[test]
fn big_in_list_5000_literals() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=100_000i64 {
        db.execute(
            &format!("INSERT INTO t (val) VALUES ({})", (i * 37) % 100_000),
            [],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // 4000 present + 1000 absent values.
    let list: Vec<String> = (0..5000)
        .map(|i| {
            if i % 5 == 0 {
                format!("{}", 1_000_000 + i) // absent
            } else {
                format!("{}", (i * 91) % 100_000) // present
            }
        })
        .collect();
    let sql = format!("SELECT COUNT(*) FROM t WHERE val IN ({})", list.join(", "));
    let expected: i64 = (0..5000)
        .filter(|i| i % 5 != 0)
        .map(|i| (i * 91) % 100_000)
        .collect::<std::collections::HashSet<i64>>()
        .len() as i64;

    // Same SQL text 10x: the cache holds ONE plan (Arc-shared list) and
    // every execution must produce the identical count.
    let mut counts = Vec::with_capacity(10);
    for _ in 0..10 {
        let rows = db.query(&sql, []).unwrap();
        counts.push(rows[0][0].clone());
    }
    assert!(
        counts.iter().all(|c| *c == Value::Integer(expected)),
        "5000-literal IN list count drifted: expected {expected}, got {counts:?}"
    );
}
