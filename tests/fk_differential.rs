//! Foreign keys against the bundled SQLite, statement by statement, on
//! FILE databases (autocommit flushes each statement, so a failing one
//! must still be undone): every outcome (error text included), every
//! table's rows, and `PRAGMA foreign_key_check` must match.
//!
//! Covers fkey.c's statement-counter semantics: a row may be its own
//! parent; a later row of the same statement may supply a parent; the row
//! being deleted / updated is never its own child; an UPDATE's new values
//! are looked up after the write; the parent's affinity and collation
//! decide which children match; a REPLACE that removes a parent row is
//! settled at statement end; RELEASE of a transaction savepoint checks
//! deferred keys like COMMIT.

use rusqlite::types::Value as Sv;
use rustqlite::{Database, Value};

fn ours(db: &mut Database, sql: &str) -> Vec<String> {
    db.query(sql, [])
        .unwrap_or_else(|e| panic!("ours {sql}: {e}"))
        .into_iter()
        .map(|r| format!("{r:?}"))
        .collect()
}

fn theirs(c: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut st = c.prepare(sql).unwrap();
    let n = st.column_count();
    st.query_map([], |r| {
        Ok((0..n)
            .map(|i| match r.get::<_, Sv>(i).unwrap() {
                Sv::Null => Value::Null,
                Sv::Integer(i) => Value::Integer(i),
                Sv::Real(f) => Value::Real(f),
                Sv::Text(t) => Value::Text(t.as_str().into()),
                Sv::Blob(b) => Value::Blob(b),
            })
            .collect::<Vec<_>>())
    })
    .unwrap()
    .map(|r| format!("{:?}", r.unwrap()))
    .collect()
}

fn run(tag: &str, script: &[&str]) {
    let dir = std::env::temp_dir().join(format!("fk_diff_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut db = Database::open(dir.join("ours.db")).unwrap();
    let c = rusqlite::Connection::open(dir.join("theirs.db")).unwrap();
    for sql in std::iter::once(&"PRAGMA foreign_keys = 1").chain(script) {
        let a = db.execute(sql, []).map_err(|e| e.sqlite_message());
        let b = c.execute_batch(sql).map_err(|e| match e {
            rusqlite::Error::SqliteFailure(_, Some(m)) => m,
            e => e.to_string(),
        });
        assert_eq!(a, b, "{tag}: outcome of {sql}");
        assert_eq!(
            db.is_autocommit(),
            c.is_autocommit(),
            "{tag}: txn state after {sql}"
        );
        let tables = theirs(
            &c,
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
        );
        for t in tables {
            let name = t.trim_start_matches("[Text(\"").trim_end_matches("\")]");
            let q = format!("SELECT * FROM \"{name}\"");
            let (mut a, mut b) = (ours(&mut db, &q), theirs(&c, &q));
            a.sort();
            b.sort();
            assert_eq!(a, b, "{tag}: {name} after {sql}");
        }
        let q = "PRAGMA foreign_key_check";
        assert_eq!(
            ours(&mut db, q),
            theirs(&c, q),
            "{tag}: foreign_key_check after {sql}"
        );
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn self_referencing_rows() {
    for (i, schema) in [
        "CREATE TABLE self(a INTEGER PRIMARY KEY, b REFERENCES self(a))",
        "CREATE TABLE self(a PRIMARY KEY, b REFERENCES self(a))",
        "CREATE TABLE self(a UNIQUE, b INTEGER PRIMARY KEY REFERENCES self(a))",
        "CREATE TABLE self(a PRIMARY KEY, b REFERENCES self DEFERRABLE INITIALLY DEFERRED)",
    ]
    .into_iter()
    .enumerate()
    {
        run(
            &format!("self{i}"),
            &[
                schema,
                "INSERT INTO self VALUES(13, 13)",
                "UPDATE self SET a = 14, b = 14",
                "UPDATE self SET b = 15",
                "UPDATE self SET a = 15",
                "UPDATE self SET a = 15, b = 16",
                "UPDATE self SET a = 17, b = 17",
                "INSERT INTO self VALUES(20, 21), (21, 21)",
                "INSERT INTO self VALUES(30, 31), (32, 32)",
                "DELETE FROM self WHERE a = 21",
                "DELETE FROM self",
                "INSERT INTO self VALUES(20, 21)",
            ],
        );
    }
}

#[test]
fn parent_affinity_and_collation_decide_matches() {
    run(
        "ipk",
        &[
            "CREATE TABLE i(i INTEGER PRIMARY KEY)",
            "CREATE TABLE j(j REFERENCES i)",
            "INSERT INTO i VALUES(35)",
            "INSERT INTO j VALUES('35.0')",
            "DELETE FROM i",
        ],
    );
    run(
        "nocase",
        &[
            "CREATE TABLE i(i TEXT COLLATE nocase PRIMARY KEY)",
            "CREATE TABLE j(j TEXT COLLATE binary REFERENCES i(i))",
            "INSERT INTO i VALUES('SQLite')",
            "INSERT INTO j VALUES('sqlite')",
            "DELETE FROM i",
            "UPDATE i SET i = 'SQLITE'",
            "UPDATE i SET i = 'other'",
        ],
    );
    run(
        "cascade",
        &[
            "CREATE TABLE zeus(a INTEGER COLLATE NOCASE, b, PRIMARY KEY(a, b))",
            "CREATE TABLE apollo(c, d, FOREIGN KEY(c, d) REFERENCES zeus ON UPDATE CASCADE)",
            "INSERT INTO zeus VALUES('abc', 'xyz')",
            "INSERT INTO apollo VALUES('ABC', 'xyz')",
            "UPDATE zeus SET a = 'aBc'",
            "UPDATE zeus SET a = 1, b = 1",
        ],
    );
}

#[test]
fn replace_of_a_parent_row_settles_at_statement_end() {
    run(
        "replace",
        &[
            "CREATE TABLE pp(a UNIQUE, b, c, PRIMARY KEY(b, c))",
            "CREATE TABLE cc(d, e, f UNIQUE, FOREIGN KEY(d, e) REFERENCES pp)",
            "INSERT INTO pp VALUES(1, 2, 3)",
            "INSERT INTO cc VALUES(2, 3, 1)",
            "REPLACE INTO pp VALUES(1, 4, 5)",
            "REPLACE INTO pp(rowid, a, b, c) VALUES(1, 2, 3, 4)",
            "BEGIN",
            "REPLACE INTO pp VALUES(1, 4, 5)",
            "COMMIT",
            "REPLACE INTO pp(rowid, a, b, c) VALUES(1, 2, 2, 3)",
            "REPLACE INTO pp(rowid, a, b, c) VALUES(2, 2, 2, 3)",
        ],
    );
}

#[test]
fn rowid_spelling_in_update_set() {
    run(
        "rowid",
        &[
            "CREATE TABLE t7(a, b INTEGER PRIMARY KEY)",
            "CREATE TABLE t8(c REFERENCES t7, d)",
            "INSERT INTO t7 VALUES(1, 2)",
            "INSERT INTO t8 VALUES(2, 9)",
            "UPDATE t7 SET rowid = 5",
            "UPDATE t7 SET oid = 2",
        ],
    );
}

#[test]
fn release_of_a_transaction_savepoint_checks_deferred_keys() {
    run(
        "release",
        &[
            "CREATE TABLE node(nodeid PRIMARY KEY, parent REFERENCES node DEFERRABLE INITIALLY DEFERRED)",
            "CREATE TABLE leaf(cellid PRIMARY KEY, parent REFERENCES node DEFERRABLE INITIALLY DEFERRED)",
            "INSERT INTO node VALUES(2, NULL)",
            "SAVEPOINT outer",
            "INSERT INTO leaf VALUES('d', 3)",
            "RELEASE outer",
            "INSERT INTO leaf VALUES('e', 3)",
            "INSERT INTO node VALUES(3, 2)",
            "RELEASE outer",
            "SAVEPOINT outer",
            "INSERT INTO leaf VALUES('f', 4)",
            "SAVEPOINT inner",
            "INSERT INTO leaf VALUES('g', 4)",
            "RELEASE outer",
            "ROLLBACK TO inner",
            "COMMIT",
            "INSERT INTO node VALUES(4, NULL)",
            "RELEASE outer",
        ],
    );
}
