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

// ---------------------------------------------------------------------------
// auto_vacuum pointer-map pages: SQLite creates an auto-vacuum database,
// the engine reads + rewrites it (mode preserved, ptrmap pages emitted),
// and real SQLite verifies every pointer-map entry via integrity_check.
// ---------------------------------------------------------------------------

/// Build a source auto-vacuum database exercising every ptrmap entry
/// type: multi-level table trees (type 5 non-root pages), wide rows with
/// overflow chains (types 3/4), an index, a WITHOUT ROWID table (roots =
/// type 1) — then return the expected row checksum.
fn build_av_source(path: &std::path::Path, mode: &str) -> i64 {
    let con = rusqlite::Connection::open(path).unwrap();
    con.execute_batch(&format!("PRAGMA auto_vacuum = {mode};"))
        .unwrap();
    con.execute_batch(
        "CREATE TABLE big(id INTEGER PRIMARY KEY, pad TEXT);
         CREATE INDEX idx_big_pad ON big(pad);
         CREATE TABLE wr(k TEXT PRIMARY KEY, v INT) WITHOUT ROWID;
         CREATE TABLE ovr(id INTEGER PRIMARY KEY, wide TEXT);
         INSERT INTO ovr VALUES (1, 'x');",
    )
    .unwrap();
    let mut big_sum = 0i64;
    for i in 1..=2500i64 {
        let pad = format!("p{:04}", i % 97);
        con.execute(
            "INSERT INTO big(id, pad) VALUES (?1, ?2)",
            rusqlite::params![i, pad],
        )
        .unwrap();
        big_sum += i;
    }
    // Overflow: 9 KB of text spans 3+ overflow pages (4 KiB page size).
    let wide = "A".repeat(9217);
    con.execute(
        "INSERT INTO ovr(id, wide) VALUES (2, ?1)",
        rusqlite::params![wide],
    )
    .unwrap();
    con.execute_batch("INSERT INTO wr VALUES ('a', 1), ('b', 2), ('C', 3);")
        .unwrap();
    let ck: i64 = con
        .query_row("SELECT sum(id) FROM big", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ck, big_sum);
    drop(con);
    big_sum
}

fn verify_av_file(path: &std::path::Path, expect_mode: i64, big_sum: i64, wr_sum: i64) {
    let con = rusqlite::Connection::open(path).unwrap();
    assert_eq!(integrity_check(&con), "ok", "ptrmap entries must validate");
    let av: i64 = con
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    assert_eq!(av, expect_mode, "auto_vacuum mode must round-trip");
    let fl: i64 = con
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fl, 0, "engine output stays dense");
    let ck: i64 = con
        .query_row("SELECT sum(id) FROM big", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ck, big_sum);
    let n: i64 = con
        .query_row("SELECT count(*) FROM ovr", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "overflow rows survive");
    let wide_len: i64 = con
        .query_row("SELECT length(wide) FROM ovr WHERE id = 2", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(wide_len, 9217, "overflow chain content is byte-exact");
    let w: i64 = con
        .query_row("SELECT sum(v) FROM wr", [], |r| r.get(0))
        .unwrap();
    assert_eq!(w, wr_sum);
    // A follow-up CREATE TABLE takes largest-root+1: must not collide.
    con.execute_batch("CREATE TABLE late(x); INSERT INTO late VALUES (42);")
        .unwrap();
    assert_eq!(
        integrity_check(&con),
        "ok",
        "next-root allocation stays clean"
    );
    drop(con);
}

#[test]
fn autovacuum_full_roundtrip() {
    let path = temp_path("av_full");
    let big_sum = build_av_source(&path, "FULL");
    // The source itself must be a well-formed av db.
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        let av = con
            .query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))
            .unwrap();
        assert_eq!(av, 1);
        assert_eq!(integrity_check(&con), "ok");
    }
    // Engine: read, mutate, write back (mode preserved + ptrmap pages).
    {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(db.disk_format(), "sqlite");
        let av: Vec<Vec<Value>> = db.query("PRAGMA auto_vacuum", ()).unwrap();
        assert_eq!(av[0][0], Value::Integer(1));
        let rows = rows_of(&db, "SELECT sum(id) FROM big");
        assert_eq!(rows[0][0], Value::Integer(big_sum));
        db.execute("INSERT INTO big(id, pad) VALUES (100000, 'zz')", ())
            .unwrap();
        db.execute("DELETE FROM big WHERE id = 100000", ()).unwrap();
        db.execute("INSERT INTO wr VALUES ('d', 4)", ()).unwrap();
        db.execute("UPDATE ovr SET wide = 'y' WHERE id = 1", ())
            .unwrap();
    }
    verify_av_file(&path, 1, big_sum, 10);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn autovacuum_incremental_roundtrip() {
    let path = temp_path("av_incr");
    let big_sum = build_av_source(&path, "INCREMENTAL");
    {
        let mut db = Database::open(&path).unwrap();
        let av = rows_of(&db, "PRAGMA auto_vacuum");
        assert_eq!(av[0][0], Value::Integer(2), "INCREMENTAL reports mode 2");
        db.execute("INSERT INTO big(id, pad) VALUES (200000, 'q')", ())
            .unwrap();
    }
    // Header 64 (incremental flag) must be set: mode 2 round-trips.
    let data = std::fs::read(&path).unwrap();
    let be32 = |o: usize| u32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
    assert_ne!(be32(52), 0, "largest root set");
    assert_eq!(be32(64), 1, "incremental-vacuum flag");
    // The engine INSERT added id 200000 to the sum.
    verify_av_file(&path, 2, big_sum + 200_000, 6);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn autovacuum_pragma_roundtrip_on_empty_engine_file() {
    let path = temp_path("av_empty");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        // Fresh file: default none.
        assert_eq!(rows_of(&db, "PRAGMA auto_vacuum")[0][0], Value::Integer(0));
        // Write form (bare keyword and integer) on the EMPTY schema.
        db.execute("PRAGMA auto_vacuum = FULL", ()).unwrap();
        assert_eq!(rows_of(&db, "PRAGMA auto_vacuum")[0][0], Value::Integer(1));
        db.execute("PRAGMA auto_vacuum = 2", ()).unwrap();
        assert_eq!(rows_of(&db, "PRAGMA auto_vacuum")[0][0], Value::Integer(2));
        db.execute("PRAGMA auto_vacuum = INCREMENTAL", ()).unwrap();
        assert_eq!(rows_of(&db, "PRAGMA auto_vacuum")[0][0], Value::Integer(2));
        // Invalid values parse as NONE (SQLite's getAutoVacuum clamp).
        db.execute("PRAGMA auto_vacuum = BANANA", ()).unwrap();
        assert_eq!(rows_of(&db, "PRAGMA auto_vacuum")[0][0], Value::Integer(0));
        db.execute("PRAGMA auto_vacuum = FULL", ()).unwrap();
        // Now build content: the file materializes as auto-vacuum.
        db.execute("CREATE TABLE t(a TEXT, b INT)", ()).unwrap();
        db.execute("INSERT INTO t VALUES ('x', 1)", ()).unwrap();
        // Once content exists, the assignment is silently ignored.
        db.execute("PRAGMA auto_vacuum = NONE", ()).unwrap();
        assert_eq!(rows_of(&db, "PRAGMA auto_vacuum")[0][0], Value::Integer(1));
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let av: i64 = con
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .unwrap();
        assert_eq!(av, 1, "engine-created av file is verified by SQLite");
        let b: i64 = con
            .query_row("SELECT b FROM t WHERE a = 'x'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(b, 1);
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn non_autovacuum_rewrite_stays_dense() {
    // Regression guard: plain files must NOT gain pointer-map pages.
    let path = temp_path("av_none");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        con.execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (1),(2),(3);")
            .unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("INSERT INTO t VALUES (4)", ()).unwrap();
    }
    let data = std::fs::read(&path).unwrap();
    let be32 = |o: usize| u32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
    assert_eq!(be32(52), 0, "no largest-root without auto_vacuum");
    // Page 2 is the first object root, not a zeroed pointer-map page.
    assert_eq!(data[4096], 0x0d, "page 2 is a table leaf");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn autovacuum_ptrmap_entry_geometry() {
    // Byte-level spot check of the emitted map: page 2 is the first
    // pointer-map page; its first entry (page 3, the first object root)
    // is type 1 with parent 0.
    let path = temp_path("av_geom");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("PRAGMA auto_vacuum = FULL", ()).unwrap();
        db.execute("CREATE TABLE t(a)", ()).unwrap();
        db.execute("INSERT INTO t VALUES (1)", ()).unwrap();
        // Later commits append WAL frames; VACUUM forces the full
        // materialization (page map + header) into the main file.
        db.execute("VACUUM", ()).unwrap();
    }
    let data = std::fs::read(&path).unwrap();
    assert_eq!(data.len() % 4096, 0);
    let page2 = &data[4096..8192];
    // First entry: 5 bytes at offset 0 = page 3 (the table root).
    assert_eq!(page2[0], 1, "entry type 1 (b-tree root) for page 3");
    assert_eq!(&page2[1..5], &[0, 0, 0, 0], "root parent is 0");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// REAL WAL sidecar writes: after the first full write of a session,
// later commits append checksum-chained frames to <db>-wal. Real SQLite
// recovers and sees the committed state; the engine's own reader folds
// the same frames; torn tails stop at the last commit boundary.
// ---------------------------------------------------------------------------

#[test]
fn engine_wal_sidecar_sqlite_reads_incremental_commits() {
    let path = temp_path("wal_writer");
    let wal = rustqlite::storage::sqlitefmt::reader::wal_path_of(&path);
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", ())
            .unwrap();
        // First dump after the initial empty full write: a WAL commit.
        let w1 = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert!(w1 > 32, "CREATE TABLE commit must be WAL frames (got {w1})");
        for i in 1..=25 {
            db.execute(
                "INSERT INTO t VALUES (?1, ?2)",
                [Value::Integer(i), Value::Text(format!("v{i}").into())],
            )
            .unwrap();
        }
        let w2 = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert!(w2 > w1, "inserts append frames incrementally");
        // The main file must NOT have been rewritten (still the initial
        // full write): its size is unchanged while the sidecar grew.
        let main = std::fs::metadata(&path).unwrap().len();
        assert!(
            main > 0 && main <= 4096 * 2,
            "main file stays small: {main}"
        );
        let n: i64 = db.query("SELECT count(*) FROM t", ()).unwrap()[0][0].as_integer();
        assert_eq!(n, 25);
    }
    // REAL SQLite reads the WAL-committed state via recovery.
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 25, "SQLite must recover the engine's WAL frames");
        let v: String = con
            .query_row("SELECT v FROM t WHERE id = 25", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "v25");
    }
    // The engine's own reader folds the same frames (reopened session:
    // next dump re-establishes with a full write, data intact).
    {
        let mut db = Database::open(&path).unwrap();
        let n: i64 = db.query("SELECT count(*) FROM t", ()).unwrap()[0][0].as_integer();
        assert_eq!(n, 25);
        db.execute("INSERT INTO t VALUES (100, 'after-reopen')", ())
            .unwrap();
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        let n: i64 = con
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 26, "post-reopen commit also visible to SQLite");
        assert_eq!(integrity_check(&con), "ok");
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&wal);
}

#[test]
fn engine_wal_growth_beyond_main_file() {
    // A WAL commit that GROWS the database past the main file's length:
    // new pages live only in the sidecar; both SQLite and the engine's
    // reader must extend their page view from the commit frame's
    // db-size field.
    let path = temp_path("wal_growth");
    let wal = rustqlite::storage::sqlitefmt::reader::wal_path_of(&path);
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, pad TEXT)", ())
            .unwrap();
        let main0 = std::fs::metadata(&path).unwrap().len();
        // ~300-byte pads: 300 rows stay well under the 1000-frame
        // autocheckpoint threshold, so the growth lives ONLY in the
        // sidecar and the main file is genuinely untouched.
        for i in 0..300 {
            let pad = format!("pad-{i:04}-{}", "x".repeat(280));
            db.execute(
                "INSERT INTO t(id, pad) VALUES (?1, ?2)",
                [Value::Integer(i + 1), Value::Text(pad.into())],
            )
            .unwrap();
        }
        let n: i64 = db.query("SELECT count(*) FROM t", ()).unwrap()[0][0].as_integer();
        assert_eq!(n, 300);
        // WAL exists and the main file was NOT rewritten (checkpoint
        // threshold is 1000 frames; this workload stays below it).
        assert!(std::fs::metadata(&wal)
            .map(|m| m.len() > 32)
            .unwrap_or(false));
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            main0,
            "main untouched"
        );
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 300, "grown pages visible through the sidecar");
        let pad: String = con
            .query_row("SELECT pad FROM t WHERE id = 300", [], |r| r.get(0))
            .unwrap();
        assert!(!pad.is_empty());
    }
    {
        let db = Database::open(&path).unwrap();
        let n: i64 = db.query("SELECT count(*) FROM t", ()).unwrap()[0][0].as_integer();
        assert_eq!(n, 300, "engine's own reader folds grown WAL pages");
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&wal);
}

#[test]
fn engine_wal_torn_tail_stops_at_commit_boundary() {
    // Crash simulation: chop the sidecar mid-frame. Readers must see
    // the last COMPLETE commit, never a torn one (SQLite: frame
    // checksum/size validation; engine: full-frame + commit marker).
    let path = temp_path("wal_torn");
    let wal = rustqlite::storage::sqlitefmt::reader::wal_path_of(&path);
    let mut committed = 0i64;
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", ())
            .unwrap();
        for i in 1..=10 {
            db.execute("INSERT INTO t VALUES (?1)", [Value::Integer(i)])
                .unwrap();
            committed = i;
        }
    }
    {
        // Torn tail: cut the last frame's final 100 bytes.
        let len = std::fs::metadata(&wal).unwrap().len() as usize;
        let mut data = std::fs::read(&wal).unwrap();
        data.truncate(len - 100);
        std::fs::write(&wal, &data).unwrap();
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, committed - 1, "SQLite sees the last complete commit");
    }
    {
        let db = Database::open(&path).unwrap();
        let n: i64 = db.query("SELECT count(*) FROM t", ()).unwrap()[0][0].as_integer();
        assert_eq!(n, committed - 1, "engine sees the same boundary");
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&wal);
}

#[test]
fn engine_wal_checkpoint_pressure_folds_back() {
    // Crossing the 1000-frame autocheckpoint pressure folds the sidecar
    // into the main file (full atomic write) and starts fresh salts.
    let path = temp_path("wal_ckpt");
    let wal = rustqlite::storage::sqlitefmt::reader::wal_path_of(&path);
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", ())
            .unwrap();
        // 1200 single-row commits: each is one frame (page 1 + the
        // touched leaf) — well past 1000 frames total.
        for i in 1..=1200 {
            db.execute(
                "INSERT INTO t VALUES (?1, ?2)",
                [Value::Integer(i), Value::Text(format!("v{i}").into())],
            )
            .unwrap();
        }
        let n: i64 = db.query("SELECT count(*) FROM t", ()).unwrap()[0][0].as_integer();
        assert_eq!(n, 1200);
    }
    // After the fold the sidecar is either absent or tiny; the main
    // file holds everything.
    let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    assert!(
        wal_len < 1000 * 4120,
        "sidecar must not grow unbounded: {wal_len}"
    );
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1200);
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&wal);
}
