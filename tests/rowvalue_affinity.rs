//! Correlated `t.rowid` references from subqueries of a table without a
//! rowid alias, and comparison affinity against BLOB-affinity columns
//! (rowvalue9.test), checked row-for-row against SQLite.
use rustqlite::Database;

fn run_both(setup: &[&str], queries: &[&str]) {
    let mut db = Database::open_in_memory().unwrap();
    let con = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        db.execute(s, []).unwrap();
        con.execute_batch(s).unwrap();
    }
    for q in queries {
        let is_select = q.trim_start().to_ascii_uppercase().starts_with("SELECT");
        if !is_select {
            db.execute(q, []).unwrap();
            con.execute_batch(q).unwrap();
            continue;
        }
        let ours: Vec<String> = db
            .query(q, [])
            .unwrap()
            .iter()
            .map(|r| format!("{:?}", r.iter().map(|v| v.to_string()).collect::<Vec<_>>()))
            .collect();
        let mut stmt = con.prepare(q).unwrap();
        let n = stmt.column_count();
        let theirs: Vec<String> = stmt
            .query_map([], |r| {
                Ok((0..n)
                    .map(|i| {
                        let v: rusqlite::types::Value = r.get(i).unwrap();
                        match v {
                            rusqlite::types::Value::Null => "NULL".to_string(),
                            rusqlite::types::Value::Integer(i) => i.to_string(),
                            rusqlite::types::Value::Real(f) => {
                                rustqlite::Value::Real(f).to_string()
                            }
                            rusqlite::types::Value::Text(t) => t,
                            rusqlite::types::Value::Blob(b) => {
                                rustqlite::Value::Blob(b).to_string()
                            }
                        }
                    })
                    .collect::<Vec<_>>())
            })
            .unwrap()
            .map(|r| format!("{:?}", r.unwrap()))
            .collect();
        assert_eq!(ours, theirs, "{q}");
    }
}

#[test]
fn correlated_rowid_and_blob_affinity_match_sqlite() {
    let setup = [
        "CREATE TABLE d1(a TEXT, b INTEGER, c NUMERIC)",
        "CREATE TABLE d2(x BLOB, y BLOB)",
        "INSERT INTO d1 VALUES(1,1,1),(2,2,2),(3,3,3),(4,4,4)",
        "INSERT INTO d2 VALUES (1,1),(2,'2'),('3',3),('4','4')",
    ];
    let queries = [
        "SELECT rowid, (SELECT x FROM d2 WHERE d2.rowid = d1.rowid) FROM d1",
        "SELECT rowid, (SELECT x FROM d2 WHERE rowid = d1.oid) FROM d1",
        "SELECT rowid FROM d1 WHERE a IN (SELECT x FROM d2)",
        "SELECT rowid FROM d1 WHERE c IN (SELECT y FROM d2)",
        "SELECT rowid FROM d1 WHERE (a, c) IN (SELECT x, y FROM d2)",
        "SELECT rowid FROM d1 WHERE (c, a) IN (SELECT x, y FROM d2)",
        "SELECT rowid FROM d1 WHERE (+c, a) IN (SELECT x, y FROM d2)",
        "SELECT d1.rowid FROM d1, d2 WHERE a = y",
        "SELECT (SELECT 1, 2, 3) == (SELECT 1, 2, 3), (SELECT 1, 0) < (SELECT 1, 2)",
        "CREATE INDEX idx ON d1(c, a)",
        "SELECT rowid FROM d1 WHERE (a, c) IN (SELECT x, y FROM d2)",
        "SELECT rowid FROM d1 WHERE (c, a) IN (SELECT x, y FROM d2)",
        "UPDATE d1 SET b = (SELECT y FROM d2 WHERE d2.rowid = d1.rowid)",
        "SELECT rowid, b FROM d1",
        "DELETE FROM d1 WHERE (SELECT x FROM d2 WHERE d2.rowid = d1.rowid) = 2",
        "SELECT rowid FROM d1",
    ];
    run_both(&setup, &queries);
}
