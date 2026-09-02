//! FILE-database persistence probe for the INSERT chain: chained inserts
//! with B+tree splits, COMMIT, reopen — the schema row must point at the
//! live root and every row must be visible.

use rustqlite::Database;

fn main() {
    let path = std::env::temp_dir().join("probe_chain_persist.db");
    let _ = std::fs::remove_file(&path);

    // Phase 1: chained inserts (multiple splits: 6000 rows x ~20B payload
    // over 4KiB pages), autocommit mode.
    {
        let mut db = Database::open(&path).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        for i in 1..=6000i64 {
            db.execute(&format!("INSERT INTO t (name, val) VALUES ('name{i}', {i})"), [])
                .unwrap();
        }
        let n = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].clone();
        assert_eq!(n, Value::Integer(6000));
        println!("autocommit chained writes: 6000 rows, file on disk");
    }
    // Phase 2: reopen — all rows visible?
    {
        let db = Database::open(&path).unwrap();
        let n = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].clone();
        assert_eq!(n, Value::Integer(6000), "rows lost after reopen!");
        let mx = db.query("SELECT MAX(id), MAX(val) FROM t", []).unwrap();
        assert_eq!(mx[0][0], Value::Integer(6000));
        assert_eq!(mx[0][1], Value::Integer(6000));
        println!("reopen after autocommit chain: 6000 rows intact");
    }

    // Phase 3: transactional chained inserts + splits + COMMIT + reopen.
    let path2 = std::env::temp_dir().join("probe_chain_persist2.db");
    let _ = std::fs::remove_file(&path2);
    {
        let mut db = Database::open(&path2).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=6000i64 {
            db.execute(&format!("INSERT INTO t (name, val) VALUES ('name{i}', {i})"), [])
                .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        let n = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].clone();
        assert_eq!(n, Value::Integer(6000));
    }
    {
        let db = Database::open(&path2).unwrap();
        let n = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].clone();
        assert_eq!(n, Value::Integer(6000), "txn rows lost after reopen!");
        let sum = db.query("SELECT SUM(val) FROM t", []).unwrap()[0][0].clone();
        assert_eq!(sum, Value::Integer(6000 * 6001 / 2));
        println!("reopen after txn chain: 6000 rows intact, sum verified");
    }

    // Phase 4: ROLLBACK of a chained transaction on a file DB — the file
    // must not contain the rolled-back rows after reopen.
    {
        let mut db = Database::open(&path2).unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 10_000..=10_500i64 {
            db.execute(&format!("INSERT INTO t (name, val) VALUES ('x{i}', {i})"), [])
                .unwrap();
        }
        db.execute("ROLLBACK", []).unwrap();
        let n = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].clone();
        assert_eq!(n, Value::Integer(6000));
    }
    {
        let db = Database::open(&path2).unwrap();
        let n = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].clone();
        assert_eq!(n, Value::Integer(6000), "rollback leaked rows to disk!");
        println!("rollback on file DB: clean");
    }

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&path2);
    println!("file persistence probe passed");
}

use rustqlite::Value;
