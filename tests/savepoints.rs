//! SAVEPOINT differential tests: rustqlite vs SQLite (rusqlite) must agree
//! on all observable outcomes (row contents + change counts).
use rustqlite::Database;

fn count(db: &mut Database, sql: &str) -> i64 {
    db.query(sql, []).unwrap()[0][0].as_integer()
}
fn count_s(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap()
}

fn setup() -> (Database, rusqlite::Connection) {
    let mut db = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let ddl = [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
        "CREATE TABLE u (id INTEGER PRIMARY KEY, w INTEGER)",
    ];
    for s in ddl {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    let seed = "INSERT INTO t (v) VALUES ('a'), ('b'), ('c')";
    db.execute(seed, []).unwrap();
    conn.execute(seed, []).unwrap();
    (db, conn)
}

#[test]
fn savepoint_basic_rollback_to() {
    let (mut db, conn) = setup();
    let script = [
        "BEGIN",
        "INSERT INTO t (v) VALUES ('x1')",
        "SAVEPOINT s1",
        "INSERT INTO t (v) VALUES ('x2')",
        "INSERT INTO t (v) VALUES ('x3')",
        "ROLLBACK TO SAVEPOINT s1",
        "INSERT INTO t (v) VALUES ('x4')",
        "COMMIT",
    ];
    for s in script {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM t"),
        count_s(&conn, "SELECT COUNT(*) FROM t")
    );
    let ours: Vec<String> = db
        .query("SELECT id FROM t ORDER BY id", [])
        .unwrap()
        .iter()
        .map(|r| format!("{}", r[0].as_integer()))
        .collect();
    let theirs: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM t ORDER BY id").unwrap();
        let rows: Vec<i64> = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows.iter().map(|i| i.to_string()).collect()
    };
    assert_eq!(ours, theirs);
    // 4 rows: a,b,c + x1 + x4 (x2,x3 rolled back)
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM t"), 5);
}

#[test]
fn savepoint_release_keeps_changes() {
    let (mut db, conn) = setup();
    let script = [
        "BEGIN",
        "SAVEPOINT s1",
        "INSERT INTO t (v) VALUES ('r1')",
        "RELEASE SAVEPOINT s1",
        "INSERT INTO t (v) VALUES ('r2')",
        "COMMIT",
    ];
    for s in script {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM t"), 5);
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM t"),
        count_s(&conn, "SELECT COUNT(*) FROM t")
    );
}

#[test]
fn savepoint_nested_rollback_inner_and_outer() {
    let (mut db, conn) = setup();
    let script = [
        "BEGIN",
        "INSERT INTO t (v) VALUES ('n1')",
        "SAVEPOINT s1",
        "INSERT INTO t (v) VALUES ('n2')",
        "SAVEPOINT s2",
        "INSERT INTO t (v) VALUES ('n3')",
        "ROLLBACK TO SAVEPOINT s2", // drops n3
        "INSERT INTO t (v) VALUES ('n4')",
        "ROLLBACK TO SAVEPOINT s1", // drops n2, n4 — keeps n1
        "COMMIT",
    ];
    for s in script {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM t"), 4); // a,b,c,n1
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM t"),
        count_s(&conn, "SELECT COUNT(*) FROM t")
    );
}

#[test]
fn savepoint_outside_transaction_commits_on_release() {
    let (mut db, conn) = setup();
    let script = [
        "SAVEPOINT sp",
        "INSERT INTO t (v) VALUES ('o1')",
        "INSERT INTO t (v) VALUES ('o2')",
        "RELEASE sp",
    ];
    for s in script {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM t"), 5);
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM t"),
        count_s(&conn, "SELECT COUNT(*) FROM t")
    );
    // The transaction is committed — a later plain ROLLBACK must NOT undo it.
    db.execute("BEGIN", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('o3')", []).unwrap();
    db.execute("ROLLBACK", []).unwrap();
    conn.execute("BEGIN", []).unwrap();
    conn.execute("INSERT INTO t (v) VALUES ('o3')", []).unwrap();
    conn.execute("ROLLBACK", []).unwrap();
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM t"), 5);
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM t"),
        count_s(&conn, "SELECT COUNT(*) FROM t")
    );
}

#[test]
fn savepoint_update_delete_rollback() {
    let (mut db, conn) = setup();
    let script = [
        "BEGIN",
        "INSERT INTO u (w) VALUES (10), (20), (30)",
        "SAVEPOINT su",
        "UPDATE t SET v = 'zz' WHERE id > 1",
        "DELETE FROM u WHERE w = 20",
        "ROLLBACK TO SAVEPOINT su",
        "COMMIT",
    ];
    for s in script {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    // UPDATE and DELETE rolled back; INSERT (pre-savepoint) survives.
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM u"), 3);
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM u"),
        count_s(&conn, "SELECT COUNT(*) FROM u")
    );
    let zz = count(&mut db, "SELECT COUNT(*) FROM t WHERE v = 'zz'");
    assert_eq!(zz, 0);
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM t"),
        count_s(&conn, "SELECT COUNT(*) FROM t")
    );
}

#[test]
fn savepoint_unknown_name_errors() {
    let (mut db, _conn) = setup();
    db.execute("BEGIN", []).unwrap();
    db.execute("SAVEPOINT real1", []).unwrap();
    let err = db.execute("ROLLBACK TO SAVEPOINT nope", []);
    assert!(err.is_err(), "unknown savepoint must error");
    // After the error the transaction + real savepoint still work.
    db.execute("INSERT INTO t (v) VALUES ('q')", []).unwrap();
    db.execute("ROLLBACK TO SAVEPOINT real1", []).unwrap();
    db.execute("COMMIT", []).unwrap();
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM t"), 3);
}

#[test]
fn savepoint_multi_page_churn() {
    // Enough rows to force page splits + freelist reuse, then roll back.
    let (mut db, conn) = setup();
    let script = [
        "BEGIN",
        "INSERT INTO u (w) SELECT id FROM t", // 3 rows
        "SAVEPOINT bulk",
    ];
    for s in script {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    for i in 0..500 {
        let sql = format!("INSERT INTO u (w) VALUES ({})", i * 7);
        db.execute(&sql, []).unwrap();
        conn.execute(&sql, []).unwrap();
    }
    db.execute("ROLLBACK TO SAVEPOINT bulk", []).unwrap();
    conn.execute("ROLLBACK TO SAVEPOINT bulk", []).unwrap();
    for i in 0..50 {
        let sql = format!("INSERT INTO u (w) VALUES ({})", 1000 + i);
        db.execute(&sql, []).unwrap();
        conn.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    conn.execute("COMMIT", []).unwrap();
    assert_eq!(count(&mut db, "SELECT COUNT(*) FROM u"), 53);
    assert_eq!(
        count(&mut db, "SELECT COUNT(*) FROM u"),
        count_s(&conn, "SELECT COUNT(*) FROM u")
    );
    // Content equality on the surviving rows.
    let ours: Vec<String> = db
        .query("SELECT w FROM u ORDER BY w", [])
        .unwrap()
        .iter()
        .map(|r| format!("{}", r[0].as_integer()))
        .collect();
    let theirs: Vec<String> = {
        let mut stmt = conn.prepare("SELECT w FROM u ORDER BY w").unwrap();
        stmt.query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(|r| r.unwrap().to_string())
            .collect()
    };
    assert_eq!(ours, theirs);
}
