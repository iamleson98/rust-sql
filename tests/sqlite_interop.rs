//! SQLite disk-format interop round-trip tests.
//!
//! rusqlite (bundled real SQLite) plays BOTH roles: it creates reference
//! `.db` files, and it verifies what the engine writes — including
//! `PRAGMA integrity_check` on every file the engine produces.

use rustqlite::{Database, Value};
use std::path::PathBuf;

fn temp_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rsql_interop_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(rustqlite::storage::sqlitefmt::reader::wal_path_of(&p));
    p
}

fn integrity_check(con: &rusqlite::Connection) -> String {
    con.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
        .unwrap()
}

fn rows_of(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, ()).unwrap()
}

// ---------------------------------------------------------------------------
// Direction A: SQLite -> rustqlite (read), rustqlite writes back, SQLite
// verifies the result.
// ---------------------------------------------------------------------------

#[test]
fn sqlite_created_file_reads_and_writes_back() {
    let path = temp_path("a_basic");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        con.execute_batch(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, score REAL, data BLOB, note);
             INSERT INTO t VALUES (1, 'alice', 3.5, x'010203', NULL);
             INSERT INTO t VALUES (2, 'bob', -0.25, x'', 'trailing');
             INSERT INTO t VALUES (10, 'zoë', 1e10, X'DEADBEEF', 'big');
             CREATE TABLE empty_t(a, b);",
        )
        .unwrap();
    }

    // rustqlite opens the SQLite file, reads it, adds a row, drops.
    {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(db.disk_format(), "sqlite");
        let rows = rows_of(&db, "SELECT id, name, score, data, note FROM t ORDER BY id");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][1], Value::Text("alice".into()));
        assert_eq!(rows[0][2], Value::Real(3.5));
        assert_eq!(rows[0][3], Value::Blob(vec![1, 2, 3]));
        assert_eq!(rows[0][4], Value::Null);
        assert_eq!(rows[2][2], Value::Real(1e10));
        assert_eq!(rows[2][3], Value::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        // Autocommit write -> dump in SQLite format.
        db.execute(
            "INSERT INTO t(id, name, score, data, note) VALUES (11, 'new', 9.0, x'FF', 'engine')",
            (),
        )
        .unwrap();
        db.execute("UPDATE t SET name = 'Alice' WHERE id = 1", ())
            .unwrap();
        db.execute("DELETE FROM t WHERE id = 2", ()).unwrap();
        let count = rows_of(&db, "SELECT count(*) FROM t");
        assert_eq!(count[0][0], Value::Integer(3));
    }

    // SQLite verifies.
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let ids: Vec<i64> = {
            let mut stmt = con.prepare("SELECT id FROM t ORDER BY id").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(ids, vec![1, 10, 11]);
        let name: String = con
            .query_row("SELECT name FROM t WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "Alice");
        // empty_t survived.
        let n: i64 = con
            .query_row("SELECT count(*) FROM empty_t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}

#[test]
fn sqlite_file_indexes_autoincrement_sequences_roundtrip() {
    let path = temp_path("a_idx_seq");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        con.execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY AUTOINCREMENT, tag TEXT UNIQUE, qty INT);
             INSERT INTO items(tag, qty) VALUES ('a', 1), ('b', 2), ('c', 3);
             DELETE FROM items WHERE tag = 'b';
             CREATE INDEX idx_qty ON items(qty DESC);
             CREATE TABLE lookup(k TEXT PRIMARY KEY, v);
             INSERT INTO lookup VALUES ('x', 10), ('y', 20);
             PRAGMA user_version = 77;
             PRAGMA application_id = 4242;",
        )
        .unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        let rows = rows_of(&db, "SELECT id, tag, qty FROM items ORDER BY id");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(1));
        assert_eq!(rows[1][0], Value::Integer(3));
        // Lookups still work through the (rebuilt) indexes.
        let v = rows_of(&db, "SELECT v FROM lookup WHERE k = 'y'");
        assert_eq!(v[0][0], Value::Integer(20));
        // UNIQUE enforcement on reload.
        let dup = db.execute("INSERT INTO items(tag, qty) VALUES ('a', 9)", ());
        assert!(dup.is_err(), "UNIQUE(tag) must be enforced after load");
        // AUTOINCREMENT continues past the old high-water mark (3).
        db.execute("INSERT INTO items(tag, qty) VALUES ('d', 4)", ())
            .unwrap();
        let new_id = rows_of(&db, "SELECT max(id) FROM items");
        assert!(matches!(&new_id[0][0], Value::Integer(i) if *i >= 4));
        // user_version is live in-session.
        let uv = rows_of(&db, "PRAGMA user_version");
        assert_eq!(uv[0][0], Value::Integer(77));
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        // sqlite_sequence preserved with the source high-water mark.
        let seq: i64 = con
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'items'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(seq >= 3, "sqlite_sequence high-water preserved (got {seq})");
        // The new row is visible to SQLite.
        let tag: String = con
            .query_row("SELECT tag FROM items WHERE qty = 4", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tag, "d");
        // UNIQUE still enforced by SQLite itself.
        let dup = con.execute("INSERT INTO items(tag, qty) VALUES ('a', 99)", ());
        assert!(dup.is_err());
        // Index-backed query plan still works.
        let plan: String = con
            .query_row(
                "EXPLAIN QUERY PLAN SELECT * FROM items WHERE qty = 2",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("idx_qty"), "index usable by SQLite: {plan}");
        let uv: i64 = con
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(uv, 77);
    }
}

#[test]
fn sqlite_file_overflow_and_large_trees() {
    let path = temp_path("a_overflow");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        con.execute_batch("CREATE TABLE big(id INTEGER PRIMARY KEY, payload TEXT, blob BLOB)")
            .unwrap();
        // Rows large enough to spill into overflow chains + a multi-page
        // tree (page_size 1024 -> a few KB payload spans many pages).
        con.execute("PRAGMA page_size = 1024", ()).unwrap();
        // page_size must be set before the first table... recreate.
        con.execute_batch(
            "DROP TABLE big;
             CREATE TABLE big(id INTEGER PRIMARY KEY, payload TEXT, blob BLOB);",
        )
        .unwrap();
        let mut big_text = String::new();
        for i in 0..5000 {
            big_text.push_str(&format!("row-{i:04}-"));
        }
        for i in 0..400i64 {
            let text = if i % 7 == 0 {
                big_text.clone()
            } else {
                format!("small-{i}")
            };
            let blob: Vec<u8> = vec![i as u8; (i % 13) as usize * 100];
            con.execute(
                "INSERT INTO big(id, payload, blob) VALUES (?1, ?2, ?3)",
                rusqlite::params![i, text, blob],
            )
            .unwrap();
        }
        con.execute_batch(
            "CREATE INDEX idx_big_head ON big(substr(payload, 1, 16));
             CREATE TABLE other(x);
             INSERT INTO other VALUES (1),(2),(3);",
        )
        .unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        let rows = rows_of(&db, "SELECT count(*), sum(id) FROM big");
        assert_eq!(rows[0][0], Value::Integer(400));
        // Overflow payloads intact: length of the big ones.
        let lens = rows_of(
            &db,
            "SELECT length(payload) FROM big WHERE id % 7 = 0 AND id < 14",
        );
        for l in &lens {
            let Value::Integer(n) = l[0] else { panic!() };
            assert!(n > 40000, "overflow payload assembled ({n} bytes)");
        }
        // Blob round-trip.
        let blob = rows_of(&db, "SELECT blob FROM big WHERE id = 9");
        match &blob[0][0] {
            Value::Blob(b) => assert_eq!(b.len(), 900),
            other => panic!("expected blob: {other:?}"),
        }
        // Multi-page tree: delete most rows, verify the remainder.
        db.execute("DELETE FROM big WHERE id >= 50", ()).unwrap();
        db.execute(
            "INSERT INTO big(id, payload, blob) VALUES (20000, 'added', x'AA')",
            (),
        )
        .unwrap();
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT count(*) FROM big", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 51);
        let payload: Option<String> = con
            .query_row("SELECT payload FROM big WHERE id = 20000", [], |r| r.get(0))
            .unwrap();
        assert_eq!(payload.as_deref(), Some("added"));
        // A rebuilt big overflow row still reads correctly through SQLite.
        let big: String = con
            .query_row("SELECT payload FROM big WHERE id = 0", [], |r| r.get(0))
            .unwrap();
        assert!(big.len() > 40000);
    }
}

#[test]
fn sqlite_file_explicit_transaction_roundtrip() {
    let path = temp_path("a_txn");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        con.execute_batch(
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); INSERT INTO t VALUES (1, 'one');",
        )
        .unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("BEGIN", ()).unwrap();
        db.execute("INSERT INTO t VALUES (2, 'two')", ()).unwrap();
        db.execute("INSERT INTO t VALUES (3, 'three')", ()).unwrap();
        // A read inside the transaction sees the pending rows.
        let n = rows_of(&db, "SELECT count(*) FROM t");
        assert_eq!(n[0][0], Value::Integer(3));
        db.execute("COMMIT", ()).unwrap();
        // The COMMIT dumped the file once.
        let n = rows_of(&db, "SELECT count(*) FROM t");
        assert_eq!(n[0][0], Value::Integer(3));
        // ROLLBACK leaves the file untouched.
        db.execute("BEGIN", ()).unwrap();
        db.execute("DELETE FROM t", ()).unwrap();
        db.execute("ROLLBACK", ()).unwrap();
        let n = rows_of(&db, "SELECT count(*) FROM t");
        assert_eq!(n[0][0], Value::Integer(3));
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
    }
}

#[test]
fn sqlite_file_views_triggers_without_rowid() {
    let path = temp_path("a_vt");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        con.execute_batch(
            "CREATE TABLE kv(k TEXT PRIMARY KEY, v INT) WITHOUT ROWID;
             INSERT INTO kv VALUES ('a', 1), ('b', 2), ('c', 3);
             CREATE TABLE log(msg TEXT);
             CREATE VIEW sums AS SELECT k, v * 2 AS doubled FROM kv;
             CREATE TRIGGER t_ins AFTER INSERT ON kv BEGIN
               INSERT INTO log(msg) VALUES ('ins:' || new.k);
             END;",
        )
        .unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        let rows = rows_of(&db, "SELECT k, v FROM kv ORDER BY k");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], Value::Text("a".into()));
        assert_eq!(rows[2][1], Value::Integer(3));
        // View works.
        let v = rows_of(&db, "SELECT doubled FROM sums WHERE k = 'b'");
        assert_eq!(v[0][0], Value::Integer(4));
        // Trigger fires on engine writes.
        db.execute("INSERT INTO kv VALUES ('d', 4)", ()).unwrap();
        let logs = rows_of(&db, "SELECT msg FROM log");
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0][0], Value::Text("ins:d".into()));
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let v: i64 = con
            .query_row("SELECT v FROM kv WHERE k = 'd'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 4);
        let v2: i64 = con
            .query_row("SELECT doubled FROM sums WHERE k = 'b'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v2, 4);
        // The trigger exists in the file and fires for SQLite too.
        con.execute("INSERT INTO kv VALUES ('e', 5)", ()).unwrap();
        let msg: String = con
            .query_row("SELECT msg FROM log ORDER BY rowid DESC LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(msg, "ins:e");
    }
}

// ---------------------------------------------------------------------------
// Direction B: rustqlite creates the SQLite-format file; SQLite reads it.
// ---------------------------------------------------------------------------

#[test]
fn engine_created_sqlite_file_verified_by_sqlite() {
    let path = temp_path("b_create");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        assert_eq!(db.disk_format(), "sqlite");
        db.execute(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT UNIQUE, score REAL)",
            (),
        )
        .unwrap();
        db.execute(
            "INSERT INTO users(id, email, score) VALUES (1, 'a@x.io', 1.5), (2, 'b@x.io', 2.5)",
            (),
        )
        .unwrap();
        db.execute("CREATE INDEX idx_score ON users(score DESC)", ())
            .unwrap();
        db.execute("CREATE TABLE wr(a, b, PRIMARY KEY(a, b)) WITHOUT ROWID", ())
            .unwrap();
        db.execute("INSERT INTO wr VALUES ('k1', 10), ('k2', 20)", ())
            .unwrap();
        db.execute(
            "CREATE VIEW top AS SELECT * FROM users WHERE score > 1.2",
            (),
        )
        .unwrap();
        db.execute("PRAGMA user_version = 5", ()).unwrap();
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT count(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        let email: String = con
            .query_row("SELECT email FROM users WHERE id = 2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(email, "b@x.io");
        // Index usable.
        let plan: String = con
            .query_row(
                "EXPLAIN QUERY PLAN SELECT * FROM users WHERE score = 1.5",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("idx_score"), "plan: {plan}");
        // UNIQUE enforced by SQLite.
        let dup = con.execute("INSERT INTO users(email) VALUES ('a@x.io')", ());
        assert!(dup.is_err());
        // WITHOUT ROWID table readable.
        let b: i64 = con
            .query_row("SELECT b FROM wr WHERE a = 'k2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(b, 20);
        // View readable.
        let c: i64 = con
            .query_row("SELECT count(*) FROM top", [], |r| r.get(0))
            .unwrap();
        assert_eq!(c, 2);
        let uv: i64 = con
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(uv, 5);
    }
    // Reopen round trip: engine reads back what it wrote.
    {
        let db = Database::open(&path).unwrap();
        assert_eq!(db.disk_format(), "sqlite");
        let rows = rows_of(&db, "SELECT email FROM users WHERE id = 1");
        assert_eq!(rows[0][0], Value::Text("a@x.io".into()));
    }
}

#[test]
fn engine_large_tree_overflow_verified() {
    let path = temp_path("b_large");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT, b BLOB)", ())
            .unwrap();
        db.execute("BEGIN", ()).unwrap();
        for i in 0..3000i64 {
            let v = if i % 5 == 0 {
                "x".repeat(3000) // overflow chain
            } else {
                format!("v{i}")
            };
            db.execute(
                "INSERT INTO t VALUES (?1, ?2, ?3)",
                vec![
                    Value::Integer(i),
                    Value::Text(v.into()),
                    Value::Blob(vec![i as u8; (i % 11) as usize * 50]),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", ()).unwrap();
        db.execute("CREATE INDEX idx_v ON t(v)", ()).unwrap();
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3000);
        let big_len: i64 = con
            .query_row("SELECT length(v) FROM t WHERE id = 500", [], |r| r.get(0))
            .unwrap();
        assert_eq!(big_len, 3000);
        // Index-based lookups.
        let id: i64 = con
            .query_row("SELECT id FROM t WHERE v = 'v7'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(id, 7);
        let blob_len: i64 = con
            .query_row("SELECT length(b) FROM t WHERE id = 9", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blob_len, 450);
    }
    // Truncate hard: delete 90% then verify file shrinks logically.
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("DELETE FROM t WHERE id % 10 != 0", ()).unwrap();
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 300);
    }
}

#[test]
fn negative_numbers_and_edge_values() {
    let path = temp_path("b_edge");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute(
            "CREATE TABLE e(a INTEGER PRIMARY KEY, i INT, r REAL, t TEXT, b BLOB)",
            (),
        )
        .unwrap();
        db.execute("BEGIN", ()).unwrap();
        let rows: Vec<Vec<Value>> = vec![
            vec![
                Value::Integer(1),
                Value::Integer(i64::MIN),
                Value::Real(f64::MIN_POSITIVE),
                Value::Text("".into()),
                Value::Blob(vec![]),
            ],
            vec![
                Value::Integer(2),
                Value::Integer(-1),
                Value::Real(-2.5),
                Value::Text("qués̈".into()),
                Value::Blob(vec![0, 255]),
            ],
            vec![
                Value::Integer(3),
                Value::Integer(0),
                Value::Real(0.0),
                Value::Text("zero".into()),
                Value::Blob(vec![0; 100]),
            ],
            vec![
                Value::Integer(4),
                Value::Integer(300),
                Value::Real(2.0),
                Value::Text("two".into()),
                Value::Blob(b"same".to_vec()),
            ],
        ];
        for r in rows {
            db.execute("INSERT INTO e VALUES (?1, ?2, ?3, ?4, ?5)", r)
                .unwrap();
        }
        db.execute("COMMIT", ()).unwrap();
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let i2: i64 = con
            .query_row("SELECT i FROM e WHERE a = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(i2, i64::MIN);
        let r2: f64 = con
            .query_row("SELECT r FROM e WHERE a = 2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(r2, -2.5);
        // Integral REAL 2.0 stored via the int-as-real optimization.
        let v: rusqlite::types::Value = con
            .query_row("SELECT r FROM e WHERE a = 4", [], |r| r.get(0))
            .unwrap();
        assert!(matches!(
            v,
            rusqlite::types::Value::Real(2.0) | rusqlite::types::Value::Integer(2)
        ));
        let b: Vec<u8> = con
            .query_row("SELECT b FROM e WHERE a = 4", [], |r| r.get(0))
            .unwrap();
        assert_eq!(b, b"same");
    }
    // Engine re-reads its own file.
    {
        let db = Database::open(&path).unwrap();
        let rows = rows_of(&db, "SELECT i FROM e WHERE a = 1");
        assert_eq!(rows[0][0], Value::Integer(i64::MIN));
    }
}

#[test]
fn dump_on_drop_and_multi_table_consistency() {
    let path = temp_path("b_drop");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("CREATE TABLE a(x INTEGER PRIMARY KEY)", ())
            .unwrap();
        db.execute("CREATE TABLE b(y)", ()).unwrap();
        db.execute("CREATE TABLE c(z TEXT)", ()).unwrap();
        for i in 0..50 {
            db.execute("INSERT INTO a VALUES (?1)", vec![Value::Integer(i)])
                .unwrap();
        }
        db.execute("INSERT INTO b VALUES ('bval')", ()).unwrap();
        // No explicit dump: Drop must persist everything.
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT count(*) FROM a", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 50);
        let y: String = con.query_row("SELECT y FROM b", [], |r| r.get(0)).unwrap();
        assert_eq!(y, "bval");
    }
}

// ---------------------------------------------------------------------------
// WAL sidecar handling: a cleanly-closed WAL db folds into the read.
// ---------------------------------------------------------------------------

#[test]
fn sqlite_wal_mode_file_reads() {
    let path = temp_path("wal");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        con.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        con.execute_batch("CREATE TABLE w(a INTEGER PRIMARY KEY, b TEXT);")
            .unwrap();
        con.execute_batch("INSERT INTO w VALUES (1, 'one'), (2, 'two');")
            .unwrap();
        // COMMIT leaves frames in the WAL (no explicit checkpoint; the
        // clean close checkpoints by default, so force frames to remain
        // by keeping the connection open until after a manual checkpoint
        // skip). A clean python/sqlite3 close usually checkpoints; to
        // exercise the reader's WAL application we reopen in WAL mode
        // and write WITHOUT closing cleanly:
        //   (simulated below by writing then reopening read-only).
        drop(con);
    }
    // Default path: the file is checkpointed at close — plain read.
    {
        let db = Database::open(&path).unwrap();
        let rows = rows_of(&db, "SELECT count(*) FROM w");
        assert_eq!(rows[0][0], Value::Integer(2));
    }
    // A second round: leave a live WAL with committed frames by using
    // two connections (writer stays open).
    {
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        writer
            .execute_batch("INSERT INTO w VALUES (3, 'three');")
            .unwrap();
        // Read while the writer is still open: the WAL holds the frame.
        let db = Database::open(&path).unwrap();
        let rows = rows_of(&db, "SELECT count(*) FROM w");
        assert_eq!(
            rows[0][0],
            Value::Integer(3),
            "WAL frames must be visible to the reader"
        );
        // The engine's own dump (had_wal) must remove the sidecars and
        // keep the content — write something to trigger it.
        let mut db = db;
        db.execute("INSERT INTO w VALUES (4, 'four')", ()).unwrap();
        drop(writer);
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT count(*) FROM w", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 4);
    }
}
