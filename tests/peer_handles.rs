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
    // A writable call adopts the peer's schema; the read-only `&self`
    // surface reports SQLITE_SCHEMA until then.
    assert!(b.query("SELECT y FROM u", ()).is_err());
    b.execute("SELECT 1", ()).unwrap();
    assert_eq!(ints(&b, "SELECT y FROM u"), vec![7]);
    b.execute("DROP TABLE u", ()).unwrap();
    a.execute("SELECT 1", ()).unwrap();
    assert!(a.query("SELECT y FROM u", ()).is_err());
    b.execute("CREATE INDEX ti ON t(x)", ()).unwrap();
    a.execute("INSERT INTO t VALUES(1)", ()).unwrap();
    assert_eq!(ints(&b, "SELECT x FROM t WHERE x = 1"), vec![1]);
    integrity_ok(&b);
}

#[test]
fn stale_transaction_commit_conflicts_instead_of_overwriting() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.db");
    let mut a = Database::open(&p).unwrap();
    a.execute("CREATE TABLE t(x)", ()).unwrap();
    a.execute("INSERT INTO t VALUES(1)", ()).unwrap();
    let mut b = Database::open(&p).unwrap();

    a.execute("BEGIN", ()).unwrap();
    assert_eq!(ints(&a, "SELECT count(*) FROM t"), vec![1]);
    // `b` commits while `a`'s transaction is open on the old state.
    b.execute("INSERT INTO t VALUES(2)", ()).unwrap();
    a.execute("INSERT INTO t VALUES(3)", ()).unwrap();
    let e = a.execute("COMMIT", ()).unwrap_err().to_string();
    assert!(e.contains("locked"), "unexpected error: {e}");

    // Nothing was lost or torn; `a` retries on the fresh state.
    a.execute("INSERT INTO t VALUES(3)", ()).unwrap();
    assert_eq!(ints(&b, "SELECT x FROM t ORDER BY x"), vec![1, 2, 3]);
    integrity_ok(&a);
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
