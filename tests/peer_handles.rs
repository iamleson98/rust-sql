//! Several `Database` handles on one rollback-journal (DELETE-mode) file
//! in one process. Each handle is its own engine with its own page
//! cache; before the peer protocol (storage::peer) the second handle
//! served stale pages and its commit silently overwrote the first's
//! (a lost update — the Tcl suite's trans-3.11 / lock-1.x shape).

use rustqlite::{Database, Value};

fn ints(db: &Database, sql: &str) -> Vec<i64> {
    db.query(sql, ())
        .unwrap()
        .into_iter()
        .map(|r| match &r[0] {
            Value::Integer(i) => *i,
            v => panic!("not an integer: {v:?}"),
        })
        .collect()
}

fn integrity_ok(db: &Database) {
    assert_eq!(
        format!("{:?}", db.query("PRAGMA integrity_check", ()).unwrap()),
        "[[Text(\"ok\")]]"
    );
}

#[test]
fn alternating_writers_see_each_others_commits() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.db");
    let mut a = Database::open(&p).unwrap();
    a.execute("CREATE TABLE t(x)", ()).unwrap();
    a.execute("INSERT INTO t VALUES(1)", ()).unwrap();
    let mut b = Database::open(&p).unwrap();
    assert_eq!(ints(&b, "SELECT x FROM t"), vec![1]);

    // An uncommitted transaction on `a` is invisible to `b`; its commit
    // is visible at b's next statement.
    a.execute("BEGIN", ()).unwrap();
    a.execute("INSERT INTO t VALUES(2)", ()).unwrap();
    assert_eq!(ints(&b, "SELECT x FROM t"), vec![1]);
    a.execute("COMMIT", ()).unwrap();
    assert_eq!(ints(&b, "SELECT x FROM t"), vec![1, 2]);

    // Alternate writes in both directions: no lost update, no rowid reuse.
    for i in 0..20 {
        b.execute(&format!("INSERT INTO t VALUES({})", 100 + i), ())
            .unwrap();
        a.execute(&format!("INSERT INTO t VALUES({})", 200 + i), ())
            .unwrap();
    }
    assert_eq!(ints(&a, "SELECT count(*) FROM t"), vec![42]);
    assert_eq!(ints(&b, "SELECT count(*) FROM t"), vec![42]);
    integrity_ok(&a);
    drop(a);
    drop(b);
    let c = Database::open(&p).unwrap();
    assert_eq!(ints(&c, "SELECT count(*) FROM t"), vec![42]);
    integrity_ok(&c);
}

#[test]
fn peer_ddl_is_visible() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.db");
    let mut a = Database::open(&p).unwrap();
    a.execute("CREATE TABLE t(x)", ()).unwrap();
    let mut b = Database::open(&p).unwrap();
    assert!(b.query("SELECT * FROM u", ()).is_err());
    a.execute("CREATE TABLE u(y)", ()).unwrap();
    a.execute("INSERT INTO u VALUES(7)", ()).unwrap();
    // The read-only `&self` surface adopts the peer's schema too.
    assert_eq!(ints(&b, "SELECT y FROM u"), vec![7]);
    b.execute("DROP TABLE u", ()).unwrap();
    assert!(a.query("SELECT y FROM u", ()).is_err());
    b.execute("CREATE INDEX ti ON t(x)", ()).unwrap();
    a.execute("INSERT INTO t VALUES(1)", ()).unwrap();
    assert_eq!(ints(&b, "SELECT x FROM t WHERE x = 1"), vec![1]);
    integrity_ok(&b);
}

#[test]
fn open_read_transaction_blocks_a_peers_commit() {
    // SQLite's SHARED lock: a transaction that has read holds it until it
    // ends, and a rollback-journal commit (in-place page rewrites) cannot
    // proceed under it — the peer gets SQLITE_BUSY, nothing is lost.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.db");
    let mut a = Database::open(&p).unwrap();
    a.execute("CREATE TABLE t(x)", ()).unwrap();
    a.execute("INSERT INTO t VALUES(1)", ()).unwrap();
    let mut b = Database::open(&p).unwrap();

    a.execute("BEGIN", ()).unwrap();
    assert_eq!(ints(&a, "SELECT count(*) FROM t"), vec![1]);
    let e = b
        .execute("INSERT INTO t VALUES(2)", ())
        .unwrap_err()
        .to_string();
    assert!(e.contains("locked"), "unexpected error: {e}");
    // a's write takes RESERVED; its commit proceeds (no peer reader).
    a.execute("INSERT INTO t VALUES(3)", ()).unwrap();
    a.execute("COMMIT", ()).unwrap();
    // b retries on the fresh state.
    b.execute("INSERT INTO t VALUES(2)", ()).unwrap();
    assert_eq!(ints(&a, "SELECT x FROM t ORDER BY x"), vec![1, 2, 3]);
    integrity_ok(&b);

    // A COMMIT refused because a peer reads keeps its transaction open.
    b.execute("BEGIN", ()).unwrap();
    b.execute("INSERT INTO t VALUES(4)", ()).unwrap();
    a.execute("BEGIN", ()).unwrap();
    assert_eq!(ints(&a, "SELECT count(*) FROM t"), vec![3]);
    let e = b.execute("COMMIT", ()).unwrap_err().to_string();
    assert!(e.contains("locked"), "unexpected error: {e}");
    a.execute("COMMIT", ()).unwrap();
    b.execute("COMMIT", ()).unwrap();
    assert_eq!(ints(&a, "SELECT count(*) FROM t"), vec![4]);
    integrity_ok(&a);
}

#[test]
fn write_transaction_holds_reserved_against_peer_writers() {
    // SQLite's RESERVED lock: the first write statement of a transaction
    // (even one that changes nothing) shuts out other writers until the
    // transaction ends; readers and deferred BEGINs are unaffected.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.db");
    let mut a = Database::open(&p).unwrap();
    a.execute("CREATE TABLE t(x)", ()).unwrap();
    a.execute("INSERT INTO t VALUES(1)", ()).unwrap();
    let mut b = Database::open(&p).unwrap();
    a.execute("BEGIN", ()).unwrap();
    a.execute("UPDATE t SET x = 0 WHERE 0", ()).unwrap();
    b.execute("BEGIN", ()).unwrap();
    let e = b.execute("UPDATE t SET x = 5", ()).unwrap_err().to_string();
    assert!(e.contains("locked"), "unexpected error: {e}");
    b.execute("ROLLBACK", ()).unwrap();
    assert!(b.execute("BEGIN IMMEDIATE", ()).is_err());
    assert_eq!(ints(&b, "SELECT x FROM t"), vec![1]);
    a.execute("ROLLBACK", ()).unwrap();
    b.execute("BEGIN IMMEDIATE", ()).unwrap();
    b.execute("UPDATE t SET x = 5", ()).unwrap();
    b.execute("COMMIT", ()).unwrap();
    assert_eq!(ints(&a, "SELECT x FROM t"), vec![5]);
}

#[test]
fn concurrent_threads_with_own_handles_lose_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.db");
    {
        let mut db = Database::open(&p).unwrap();
        db.execute("CREATE TABLE t(w, i)", ()).unwrap();
    }
    const THREADS: i64 = 4;
    const PER: i64 = 100;
    let handles: Vec<_> = (0..THREADS)
        .map(|w| {
            let p = p.clone();
            std::thread::spawn(move || {
                let mut db = Database::open(&p).unwrap();
                for i in 0..PER {
                    loop {
                        match db.execute(&format!("INSERT INTO t VALUES({w}, {i})"), ()) {
                            Ok(_) => break,
                            Err(e) if e.to_string().contains("locked") => {
                                std::thread::yield_now();
                            }
                            Err(e) => panic!("insert failed: {e}"),
                        }
                    }
                    let _ = db.query("SELECT count(*) FROM t", ()).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let db = Database::open(&p).unwrap();
    assert_eq!(ints(&db, "SELECT count(*) FROM t"), vec![THREADS * PER]);
    assert_eq!(
        ints(&db, "SELECT count(DISTINCT w * 1000 + i) FROM t"),
        vec![THREADS * PER]
    );
    integrity_ok(&db);
}

// ---- WAL mode: handles take turns holding the writer role ----

fn wal_db(p: &std::path::Path) -> Database {
    let mut db = Database::open(p).unwrap();
    db.execute("PRAGMA journal_mode = WAL", ()).unwrap();
    db
}

#[test]
fn wal_alternating_writers() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.db");
    let mut a = wal_db(&p);
    a.execute("CREATE TABLE t(x)", ()).unwrap();
    // `a` wrote first and is now idle; `b` must be able to write.
    let mut b = Database::open(&p).unwrap();
    for i in 0..30 {
        b.execute(&format!("INSERT INTO t VALUES({})", 100 + i), ())
            .unwrap();
        a.execute(&format!("INSERT INTO t VALUES({})", 200 + i), ())
            .unwrap();
    }
    assert_eq!(ints(&a, "SELECT count(*) FROM t"), vec![60]);
    assert_eq!(ints(&b, "SELECT count(*) FROM t"), vec![60]);
    integrity_ok(&b);
    drop(a);
    drop(b);
    let c = Database::open(&p).unwrap();
    assert_eq!(ints(&c, "SELECT count(*) FROM t"), vec![60]);
    integrity_ok(&c);
}

#[test]
fn wal_open_transaction_keeps_the_writer_role() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.db");
    let mut a = wal_db(&p);
    a.execute("CREATE TABLE t(x)", ()).unwrap();
    let mut b = Database::open(&p).unwrap();
    a.execute("BEGIN", ()).unwrap();
    a.execute("INSERT INTO t VALUES(1)", ()).unwrap();
    // A peer's write waits out (here: fails fast with) the open transaction.
    let e = b
        .execute("INSERT INTO t VALUES(2)", ())
        .unwrap_err()
        .to_string();
    assert!(e.contains("locked"), "unexpected: {e}");
    a.execute("COMMIT", ()).unwrap();
    b.execute("INSERT INTO t VALUES(2)", ()).unwrap();
    assert_eq!(ints(&a, "SELECT x FROM t ORDER BY x"), vec![1, 2]);
    // Rolled-back work releases the role as well.
    b.execute("BEGIN", ()).unwrap();
    b.execute("INSERT INTO t VALUES(3)", ()).unwrap();
    b.execute("ROLLBACK", ()).unwrap();
    a.execute("INSERT INTO t VALUES(4)", ()).unwrap();
    assert_eq!(ints(&b, "SELECT x FROM t ORDER BY x"), vec![1, 2, 4]);
    integrity_ok(&a);
}

#[test]
fn wal_peer_ddl_and_returning_and_prepared_writes() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.db");
    let mut a = wal_db(&p);
    a.execute("CREATE TABLE t(x)", ()).unwrap();
    let mut b = Database::open(&p).unwrap();
    b.execute("CREATE TABLE u(y)", ()).unwrap();
    // `a` adopts `b`'s schema change when it next writes.
    a.execute("INSERT INTO u VALUES(5)", ()).unwrap();
    // DML-RETURNING through the query surface takes turns too.
    let r = b.query("INSERT INTO t VALUES(9) RETURNING x", ()).unwrap();
    assert_eq!(r, vec![vec![Value::Integer(9)]]);
    let r = a.query("INSERT INTO t VALUES(10) RETURNING x", ()).unwrap();
    assert_eq!(r, vec![vec![Value::Integer(10)]]);
    assert_eq!(ints(&b, "SELECT y FROM u"), vec![5]);
    assert_eq!(ints(&a, "SELECT x FROM t ORDER BY x"), vec![9, 10]);
    integrity_ok(&a);
}

#[test]
fn wal_threads_with_own_handles_lose_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.db");
    {
        let mut db = wal_db(&p);
        db.execute("CREATE TABLE t(w, i)", ()).unwrap();
    }
    const THREADS: i64 = 4;
    const PER: i64 = 100;
    let handles: Vec<_> = (0..THREADS)
        .map(|w| {
            let p = p.clone();
            std::thread::spawn(move || {
                let mut db = Database::open(&p).unwrap();
                for i in 0..PER {
                    loop {
                        match db.execute(&format!("INSERT INTO t VALUES({w}, {i})"), ()) {
                            Ok(_) => break,
                            Err(e) if e.to_string().contains("locked") => {
                                std::thread::yield_now();
                            }
                            Err(e) => panic!("insert failed: {e}"),
                        }
                    }
                    let _ = db.query("SELECT count(*) FROM t", ()).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let db = Database::open(&p).unwrap();
    assert_eq!(ints(&db, "SELECT count(*) FROM t"), vec![THREADS * PER]);
    assert_eq!(
        ints(&db, "SELECT count(DISTINCT w * 1000 + i) FROM t"),
        vec![THREADS * PER]
    );
    integrity_ok(&db);
}

#[test]
fn handle_opened_before_the_file_exists_adopts_the_peers_creation() {
    // Two handles open on a path that does not exist yet (the Tcl suite's
    // `sqlite3 db test.db; sqlite3 db2 test.db` shape): the first to write
    // creates the file, the other must adopt it, not keep its own empty one.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("new.db");
    let mut a = Database::open(&p).unwrap();
    let mut b = Database::open(&p).unwrap();
    assert!(ints(&b, "SELECT count(*) FROM sqlite_master") == vec![0]);
    a.execute("CREATE TABLE t(x)", ()).unwrap();
    a.execute("INSERT INTO t VALUES(1)", ()).unwrap();
    assert_eq!(ints(&b, "SELECT x FROM t"), vec![1]);
    b.execute("INSERT INTO t VALUES(2)", ()).unwrap();
    assert_eq!(ints(&a, "SELECT x FROM t ORDER BY x"), vec![1, 2]);
    integrity_ok(&a);
}

#[test]
fn autocommit_update_and_delete_reach_the_file_at_statement_end() {
    // The fused in-place UPDATE/DELETE patchers noted their page in the
    // dirty set without bumping the dirty counter `flush` gates on, so a
    // committed autocommit statement stayed in the cache until some later
    // write flushed it (lost on a crash; invisible to every other
    // handle). A leaked handle stands in for the crash.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("d.db");
    let mut a = Database::open(&p).unwrap();
    a.execute("CREATE TABLE t1(a int, b int)", ()).unwrap();
    a.execute("INSERT INTO t1 VALUES(1,2)", ()).unwrap();
    a.execute("INSERT INTO t1 VALUES(3,4)", ()).unwrap();
    a.execute("UPDATE t1 SET a = 100", ()).unwrap();
    let c = Database::open(&p).unwrap();
    assert_eq!(ints(&c, "SELECT a FROM t1 ORDER BY b"), vec![100, 100]);
    drop(c);
    a.execute("DELETE FROM t1 WHERE b = 2", ()).unwrap();
    let c = Database::open(&p).unwrap();
    assert_eq!(ints(&c, "SELECT b FROM t1"), vec![4]);
    drop(c);
    std::mem::forget(a);
    let c = Database::open(&p).unwrap();
    assert_eq!(ints(&c, "SELECT b FROM t1"), vec![4]);
}
