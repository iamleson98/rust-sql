//! Delete-and-append churn must not grow a B-tree without bound.
//! `INSERT OR REPLACE` on a rowid table deletes the conflicting row from
//! the middle of the tree and appends its replacement at the right edge,
//! so old leaves thin out to a row or two without ever emptying — and
//! were never merged: SQLite's savepoint6 workload grew the table to
//! 1,280 leaves for 751 rows (SQLite stays near 230 pages in all), and
//! every scan slowed with it. Underfull leaves now merge into a sibling
//! (SQLite's balance_nonroot after a delete).

use rustqlite::{Database, Value};

fn churn(db: &mut Database, ops: usize) {
    let mut seed: u64 = 1;
    for _ in 0..ops {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let x = (seed >> 33) % 1000;
        let n = 250 + (seed >> 20) % 250;
        if seed & 3 != 0 {
            db.execute(
                &format!("INSERT OR REPLACE INTO t1 VALUES({x}, printf('%.*c', {n}, 'y'))"),
                (),
            )
            .unwrap();
        } else {
            db.execute(&format!("DELETE FROM t1 WHERE x = {x}"), ())
                .unwrap();
        }
    }
}

fn scalar(db: &Database, sql: &str) -> i64 {
    match db.query(sql, ()).unwrap()[0][0] {
        Value::Integer(v) => v,
        ref other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn replace_churn_keeps_the_tree_bounded() {
    for in_txn in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Database::open(dir.path().join("t.db")).unwrap();
        db.execute("CREATE TABLE t1(x, y)", ()).unwrap();
        db.execute("CREATE UNIQUE INDEX i1 ON t1(x)", ()).unwrap();
        db.execute("CREATE INDEX i2 ON t1(y)", ()).unwrap();
        if in_txn {
            db.execute("SAVEPOINT one", ()).unwrap();
        }
        churn(&mut db, 10_000);
        let after_10k = scalar(&db, "PRAGMA page_count");
        churn(&mut db, 30_000);
        let after_40k = scalar(&db, "PRAGMA page_count");
        // ~750 rows of ~400 bytes: SQLite holds this in ~230 pages; the
        // unmerged tree was past 1,100 by 40k operations, still growing.
        assert!(
            after_40k < 450,
            "in_txn={in_txn}: {after_40k} pages after 40k ops"
        );
        assert!(
            after_40k <= after_10k + 40,
            "in_txn={in_txn}: still growing ({after_10k} -> {after_40k} pages)"
        );
        let r = db.query("PRAGMA integrity_check", ()).unwrap();
        assert_eq!(format!("{r:?}"), "[[Text(\"ok\")]]");
        let rows = scalar(&db, "SELECT count(*) FROM t1");
        assert_eq!(scalar(&db, "SELECT count(*) FROM t1 INDEXED BY i2"), rows);
        if in_txn {
            db.execute("RELEASE one", ()).unwrap();
            let r = db.query("PRAGMA integrity_check", ()).unwrap();
            assert_eq!(format!("{r:?}"), "[[Text(\"ok\")]]");
        }
    }
}
