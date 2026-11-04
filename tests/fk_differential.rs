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
        let b = c.execute_batch(sql).map_err(|e| {
            let m = match e {
                rusqlite::Error::SqliteFailure(_, Some(m)) => m,
                e => e.to_string(),
            };
            // Newer SQLite appends the statement and error offset.
            match m.find(" in CREATE ") {
                Some(i) => m[..i].to_string(),
                None => m,
            }
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
        let q = "SELECT changes(), total_changes()";
        assert_eq!(
            ours(&mut db, q),
            theirs(&c, q),
            "{tag}: change counts after {sql}"
        );
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

/// VACUUM rebuilds the store; PRAGMA foreign_keys (a connection setting)
/// must survive it — enforcement silently stopped after a VACUUM.
#[test]
fn foreign_keys_survive_vacuum() {
    run(
        "vacuum",
        &[
            "CREATE TABLE t1(a REFERENCES t2(c), b)",
            "CREATE TABLE t2(c UNIQUE, b)",
            "INSERT INTO t2 VALUES(1, 2)",
            "INSERT INTO t1 VALUES(1, 2)",
            "VACUUM",
            "INSERT INTO t1 VALUES(9, 9)",
            "DELETE FROM t2",
        ],
    );
}

/// A trigger WHEN clause may hold a subquery (e_fkey-31: the trigger
/// supplies the missing parent before the statement's FK check).
#[test]
fn trigger_when_subquery_supplies_parent() {
    run(
        "when",
        &[
            "CREATE TABLE king(a, b, PRIMARY KEY(a))",
            "CREATE TABLE prince(c REFERENCES king, d)",
            "INSERT INTO prince VALUES(1, 2)",
            "CREATE TRIGGER kt AFTER INSERT ON prince WHEN NOT EXISTS (SELECT a FROM king WHERE a = new.c) BEGIN INSERT INTO king VALUES(new.c, NULL); END",
            "INSERT INTO prince VALUES(1, 2)",
            "INSERT INTO prince VALUES(1, 3)",
            "BEGIN",
            "INSERT INTO prince VALUES(2, 3)",
            "DROP TRIGGER kt",
            "INSERT INTO prince VALUES(3, 4)",
            "COMMIT",
        ],
    );
}

/// sqlite3CreateForeignKey's definition errors (regardless of the
/// pragma) and ADD COLUMN's REFERENCES-with-default rule.
#[test]
fn foreign_key_definition_errors() {
    run(
        "defs",
        &[
            "CREATE TABLE p(x, y, PRIMARY KEY(x, y))",
            "CREATE TABLE child1(a, b, FOREIGN KEY(a, b) REFERENCES p(c))",
            "CREATE TABLE child2(a, b, FOREIGN KEY(a, c) REFERENCES p(c, d))",
            "CREATE TABLE child3(a, b, FOREIGN KEY(c, b) REFERENCES t2(d))",
            "CREATE TABLE c(jj REFERENCES p(x, y))",
            "CREATE TABLE c2(a, b, FOREIGN KEY(a,b) REFERENCES p)",
            "CREATE TABLE t1(a PRIMARY KEY)",
            "CREATE TABLE t2(a, b)",
            "ALTER TABLE t2 ADD COLUMN c REFERENCES t1",
            "ALTER TABLE t2 ADD COLUMN d DEFAULT NULL REFERENCES t1",
            "ALTER TABLE t2 ADD COLUMN e REFERENCES t1 DEFAULT 1",
            "INSERT INTO t2(a, b) VALUES(1, 2)",
            "ALTER TABLE t2 ADD COLUMN f REFERENCES t1 DEFAULT 1",
            "ALTER TABLE t2 ADD COLUMN g DEFAULT NULL REFERENCES t1",
            "PRAGMA foreign_keys = 0",
            "ALTER TABLE t2 ADD COLUMN h REFERENCES t1 DEFAULT 2",
        ],
    );
}

/// NEW / OLD inside a trigger body's subqueries (scalar, EXISTS, IN,
/// FROM-subquery, JOIN ON) bind to the trigger row — they were left
/// unbound, so the subquery saw NULL.
#[test]
fn trigger_new_old_inside_subqueries() {
    run(
        "subq",
        &[
            "CREATE TABLE src(k, v)",
            "CREATE TABLE log(a, b, c, d, e)",
            "INSERT INTO src VALUES(1, 'one'), (2, 'two'), (3, 'three')",
            "CREATE TABLE t(x, y)",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
               INSERT INTO log VALUES( \
                 (SELECT v FROM src WHERE k = new.x), \
                 EXISTS (SELECT 1 FROM src WHERE k = new.y), \
                 new.x IN (SELECT k FROM src WHERE k <> new.y), \
                 (SELECT count(*) FROM (SELECT * FROM src WHERE k <= new.x)), \
                 (SELECT s2.v FROM src s1 JOIN src s2 ON s2.k = s1.k + new.y WHERE s1.k = new.x)); \
             END",
            "CREATE TRIGGER td AFTER DELETE ON t BEGIN \
               UPDATE src SET v = (SELECT v || '!' FROM src WHERE k = old.x) WHERE k = old.y; \
             END",
            "INSERT INTO t VALUES(1, 1)",
            "INSERT INTO t VALUES(2, 9)",
            "INSERT INTO t VALUES(3, 1)",
            "DELETE FROM t WHERE x = 1",
        ],
    );
}

/// FK actions are UPDATE / DELETE programs on the child: the rewritten
/// row's CHECK / NOT NULL constraints and its own foreign keys apply,
/// index entries move with it, the statement undoes them on failure, and
/// their rows count toward total_changes() only.
#[test]
fn fk_actions_behave_like_child_statements() {
    run(
        "check",
        &[
            "CREATE TABLE ab(a PRIMARY KEY, b)",
            "CREATE TABLE cd(c PRIMARY KEY REFERENCES ab ON UPDATE CASCADE ON DELETE CASCADE, d)",
            "CREATE TABLE ef(e REFERENCES cd ON UPDATE CASCADE, f, CHECK (e!=5))",
            "INSERT INTO ab VALUES(1, 'b')",
            "INSERT INTO cd VALUES(1, 'd')",
            "INSERT INTO ef VALUES(1, 'e')",
            "UPDATE ab SET a = 5",
            "BEGIN",
            "UPDATE ab SET a = 5",
            "COMMIT",
            "BEGIN",
            "DELETE FROM ab",
            "COMMIT",
            "UPDATE ab SET a = 2",
            "DELETE FROM ef",
            "DELETE FROM ab",
        ],
    );
    run(
        "setdefault",
        &[
            "CREATE TABLE t1(a INTEGER PRIMARY KEY, b)",
            "CREATE TABLE t2(c INTEGER PRIMARY KEY, d INTEGER DEFAULT 1 REFERENCES t1 ON DELETE SET DEFAULT)",
            "CREATE INDEX t2d ON t2(d)",
            "INSERT INTO t1 VALUES(1, 'one')",
            "INSERT INTO t1 VALUES(2, 'two')",
            "INSERT INTO t2 VALUES(1, 2)",
            "DELETE FROM t1 WHERE a = 2",
            "INSERT INTO t1 VALUES(2, 'two')",
            "UPDATE t2 SET d = 2",
            "DELETE FROM t1 WHERE a = 1",
            "DELETE FROM t1",
            "SELECT * FROM t2 WHERE d = 2",
        ],
    );
    run(
        "setnull",
        &[
            "CREATE TABLE p(a PRIMARY KEY)",
            "CREATE TABLE c(x REFERENCES p ON DELETE SET NULL ON UPDATE SET NULL, y NOT NULL DEFAULT 0)",
            "CREATE TABLE n(x NOT NULL REFERENCES p ON DELETE SET NULL)",
            "CREATE INDEX cx ON c(x)",
            "INSERT INTO p VALUES(1), (2), (3)",
            "INSERT INTO c VALUES(1, 1), (2, 2), (2, 3)",
            "INSERT INTO n VALUES(3)",
            "DELETE FROM p WHERE a = 2",
            "UPDATE p SET a = 10 WHERE a = 1",
            "DELETE FROM p WHERE a = 3",
            "SELECT count(*) FROM c WHERE x IS NULL",
        ],
    );
}
