// Probe: view-DML routing after the CachedStmt.view_target refactor.
// Covers every path the raw-SQL probe used to guard, plus the
// cache-disabled (capacity 0) mode that was broken before.
// Run: cargo run --release --example probe_view_routing
use rustqlite::{Database, Value};

fn scalar_i64(db: &Database, sql: &str) -> i64 {
    let rows = db.query(sql, []).unwrap();
    if let Some(Value::Integer(i)) = rows.first().and_then(|r| r.first().cloned()) {
        return i;
    }
    i64::MIN
}

fn scalar_text(db: &Database, sql: &str) -> String {
    let rows = db.query(sql, []).unwrap();
    if let Some(Value::Text(s)) = rows.first().and_then(|r| r.first().cloned()) {
        return s.to_string();
    }
    String::new()
}

fn main() {
    let mut fails = 0;
    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond {
                println!("ok   {}", $name);
            } else {
                fails += 1;
                println!("FAIL {}", $name);
            }
        };
    }

    // ---- 1. execute-path INSERT/UPDATE/DELETE on a view with INSTEAD OF
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a INTEGER, b TEXT)", [])
        .unwrap();
    db.execute("CREATE VIEW v AS SELECT a, b FROM t", [])
        .unwrap();
    db.execute(
        "CREATE TRIGGER v_ins INSTEAD OF INSERT ON v BEGIN INSERT INTO t VALUES (NEW.a, NEW.b); END",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TRIGGER v_upd INSTEAD OF UPDATE ON v BEGIN UPDATE t SET a=NEW.a, b=NEW.b WHERE a=OLD.a; END",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TRIGGER v_del INSTEAD OF DELETE ON v BEGIN DELETE FROM t WHERE a=OLD.a; END",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO v VALUES (1, 'x')", []).unwrap();
    db.execute("INSERT INTO v (b, a) VALUES ('y', 2)", [])
        .unwrap();
    check!(
        "view insert (positional + named cols)",
        scalar_i64(&db, "SELECT COUNT(*) FROM t") == 2
    );

    // Parameterized INSERT into view (the shape that must route through
    // get_or_cache_stmt's view_target, never the table fast paths).
    db.execute(
        "INSERT INTO v VALUES (?, ?)",
        [Value::Integer(3), Value::Text("z".into())],
    )
    .unwrap();
    check!(
        "parameterized view insert",
        scalar_i64(&db, "SELECT COUNT(*) FROM t") == 3
    );

    db.execute("UPDATE v SET b = 'upd' WHERE a = 2", [])
        .unwrap();
    check!(
        "view update",
        scalar_text(&db, "SELECT b FROM t WHERE a = 2") == "upd"
    );

    db.execute("DELETE FROM v WHERE a = 1", []).unwrap();
    check!(
        "view delete",
        scalar_i64(&db, "SELECT COUNT(*) FROM t") == 2
    );

    // ---- 3. view DML without INSTEAD OF trigger errors (SQLite text)
    let mut db2 = Database::open_in_memory().unwrap();
    db2.execute("CREATE TABLE t (a INTEGER)", []).unwrap();
    db2.execute("CREATE VIEW v AS SELECT a FROM t", []).unwrap();
    let err = db2.execute("INSERT INTO v VALUES (1)", []).unwrap_err();
    println!("info: no-trigger err = {err}");
    check!(
        "view DML without trigger errors",
        err.to_string().contains("cannot modify")
    );

    // ---- 4. unknown table insert: errors via the general path (the
    // scanner fall-through no longer short-circuits with its own text).
    // NOTE: the message text ("not found: table: X") is pre-existing and
    // differs from SQLite's "no such table: X" — tracked as a remaining
    // error-parity gap in the README.
    let mut db3 = Database::open_in_memory().unwrap();
    let err = db3
        .execute("INSERT INTO nosuch VALUES (1)", [])
        .unwrap_err();
    println!("info: unknown-table err = {err}");
    check!(
        "unknown table insert errors",
        err.to_string().contains("table")
    );
    let err2 = db3
        .execute("INSERT INTO nosuch (a) VALUES (1)", [])
        .unwrap_err();
    check!(
        "unknown table (column-list shape) errors",
        err2.to_string().contains("table")
    );

    // ---- 5. cache-disabled (capacity 0) view DML — was broken at HEAD
    let mut db4 = Database::open_in_memory().unwrap();
    db4.set_stmt_cache_capacity(0);
    db4.execute("CREATE TABLE t (a INTEGER)", []).unwrap();
    db4.execute("CREATE VIEW v AS SELECT a FROM t", []).unwrap();
    db4.execute(
        "CREATE TRIGGER v_ins INSTEAD OF INSERT ON v BEGIN INSERT INTO t VALUES (NEW.a); END",
        [],
    )
    .unwrap();
    match db4.execute("INSERT INTO v VALUES (5)", []) {
        Ok(()) => check!(
            "cache-disabled view insert",
            scalar_i64(&db4, "SELECT COUNT(*) FROM t") == 1
        ),
        Err(e) => {
            println!("info: cache-disabled err = {e}");
            check!("cache-disabled view insert", false);
        }
    }

    // ---- 6. hot loop over a real table still works (S12 shape)
    let mut db5 = Database::open_in_memory().unwrap();
    db5.execute("CREATE TABLE m (id INTEGER PRIMARY KEY, a INTEGER)", [])
        .unwrap();
    db5.execute("CREATE INDEX ia ON m(a)", []).unwrap();
    db5.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        db5.execute(
            "INSERT INTO m (id, a) VALUES (?, ?)",
            [Value::Integer(i), Value::Integer(i * 3)],
        )
        .unwrap();
    }
    db5.execute("COMMIT", []).unwrap();
    check!(
        "10k indexed parameterized inserts",
        scalar_i64(&db5, "SELECT COUNT(*) FROM m WHERE a = 9") == 1
    );

    // ---- 7. DDL between hot inserts (cache invalidation staleness)
    let mut db6 = Database::open_in_memory().unwrap();
    db6.execute("CREATE TABLE m (a INTEGER)", []).unwrap();
    db6.execute("INSERT INTO m VALUES (1)", []).unwrap(); // cached: table DML
    db6.execute("DROP TABLE m", []).unwrap();
    db6.execute("CREATE VIEW m AS SELECT 1 AS a", []).unwrap();
    db6.execute(
        "CREATE TRIGGER m_ins INSTEAD OF INSERT ON m BEGIN SELECT 1; END",
        [],
    )
    .unwrap();
    let r = db6.execute("INSERT INTO m VALUES (1)", []);
    check!("drop-table + create-view re-routes DML", r.is_ok());

    if fails == 0 {
        println!("== ALL VIEW-ROUTING CHECKS PASSED ==");
    } else {
        println!("== {fails} CHECKS FAILED ==");
        std::process::exit(1);
    }
}
