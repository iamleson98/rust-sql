//! A big transaction on a file database with a small page cache spills
//! dirty pages to the uncommitted spill (SQLite's trans2.test shape).
//! Two bugs lived here:
//! - `PRAGMA integrity_check` inside the transaction FLUSHED the dirty
//!   pages into the main file — which must keep its pre-BEGIN bytes until
//!   COMMIT — so a later ROLLBACK left trees pointing past the end of the
//!   file ("page N out of range", "short read on overflow page");
//! - overflow chains read a spilled page from the main file (its
//!   pre-transaction bytes, or a short read past its end) instead of the
//!   spill.

use rustqlite::{Database, Value};

fn blob_rows(db: &Database) -> (i64, i64) {
    let r = db
        .query(
            "SELECT count(*), sum(length(z) + unicode(substr(z, -1, 1))) FROM t1",
            (),
        )
        .unwrap();
    match (&r[0][0], &r[0][1]) {
        (Value::Integer(a), Value::Integer(b)) => (*a, *b),
        other => panic!("unexpected {other:?}"),
    }
}

fn integrity(db: &Database) -> String {
    let r = db.query("PRAGMA integrity_check", ()).unwrap();
    format!("{:?}", r)
}

#[test]
fn rollback_after_spill_and_integrity_check_keeps_the_file_intact() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("t.db")).unwrap();
    db.execute("PRAGMA cache_size=100", ()).unwrap();
    db.execute(
        "CREATE TABLE t1(id INTEGER PRIMARY KEY, u1 TEXT UNIQUE, z BLOB NOT NULL, u2 TEXT UNIQUE)",
        (),
    )
    .unwrap();
    let mut seed: u64 = 1;
    let mut next = || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        seed >> 33
    };
    for i in 0..400 {
        let (a, b, n) = (next(), next(), 1000 + next() % 5000);
        db.execute(
            &format!(
                "INSERT INTO t1 VALUES({i}, 'u{a}', CAST(zeroblob({n}) || 'x' AS BLOB), 'v{b}')"
            ),
            (),
        )
        .unwrap();
    }
    db.execute("DELETE FROM t1 WHERE id % 10 = 3", ()).unwrap();
    let before = blob_rows(&db);
    assert_eq!(integrity(&db), "[[Text(\"ok\")]]");

    db.execute("BEGIN", ()).unwrap();
    for i in 400..520 {
        let (a, b, n) = (next(), next(), 1000 + next() % 5000);
        db.execute(
            &format!(
                "INSERT INTO t1 VALUES({i}, 'w{a}', CAST(zeroblob({n}) || 'y' AS BLOB), 'x{b}')"
            ),
            (),
        )
        .unwrap();
    }
    // Reads inside the transaction see the spilled overflow pages.
    let (n, _) = blob_rows(&db);
    assert_eq!(n, before.0 + 120);
    assert_eq!(integrity(&db), "[[Text(\"ok\")]]");
    db.execute("ROLLBACK", ()).unwrap();

    assert_eq!(integrity(&db), "[[Text(\"ok\")]]");
    assert_eq!(blob_rows(&db), before);
}

/// SAVEPOINT / ROLLBACK TO / DELETE inside transactions whose inserts
/// move the table's root (SQLite's pager2.test sequence): ROLLBACK TO
/// reverted the schema rows' pages but not the record of which root they
/// hold, so COMMIT skipped rewriting the root and the file kept a stale
/// one — a second connection (or a reopen) read the wrong tree.
#[test]
fn rollback_to_savepoint_keeps_committed_roots_right() {
    const SEQ: &str = "100 x 0 100 x 70 22 96 59 96 50 22 56 21 16 37 64 43 40 0 38 22 38 55 0 6 \
        43 62 32 93 54 18 13 29 45 66 29 25 61 31 53 82 75 25 96 86 10 69 2 29 6 60 80 95 \
        42 82 85 50 68 96 90 39 78 69 87 97 48 74 65 43 x";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let mut db = Database::open(&path).unwrap();
    db.execute("PRAGMA cache_size = 10", ()).unwrap();
    db.execute("CREATE TABLE t1(i INTEGER PRIMARY KEY, j BLOB)", ())
        .unwrap();
    let (mut now, mut low) = (0i64, 0i64);
    for tok in SEQ.split_whitespace() {
        if tok == "x" {
            if !db.is_autocommit() {
                db.execute("COMMIT", ()).unwrap();
            }
            db.execute("BEGIN", ()).unwrap();
            low = now;
            let other = Database::open(&path).unwrap();
            let r = other
                .query("SELECT COALESCE(max(i), 0) FROM t1", ())
                .unwrap();
            assert_eq!(r[0][0], Value::Integer(low));
            assert_eq!(
                format!("{:?}", other.query("PRAGMA integrity_check", ()).unwrap()),
                "[[Text(\"ok\")]]"
            );
            continue;
        }
        let x: i64 = tok.parse().unwrap();
        if now > x {
            if x >= low {
                db.execute(&format!("ROLLBACK TO sp_{x}"), ()).unwrap();
            } else {
                db.execute(&format!("DELETE FROM t1 WHERE i > {x}"), ())
                    .unwrap();
                low = x;
            }
        } else {
            for k in now..x {
                db.execute(&format!("SAVEPOINT sp_{k}"), ()).unwrap();
                db.execute("INSERT INTO t1(j) VALUES(randomblob(1500))", ())
                    .unwrap();
            }
        }
        now = x;
    }
}
