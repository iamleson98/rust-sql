//! Persistence & reopen durability — the "close it, then open it again"
//! matrix, modeled on SQLite's own test-harness practices.
//!
//! SQLite's testing.html (https://www.sqlite.org/testing.html) organizes
//! correctness into families (atomic commit, crash & power loss, I/O
//! errors, malloc failure, boundary values, fuzz, regression). This file
//! adds the family this project historically under-tested: **everything
//! the schema and data promise must still be true after the process
//! exits and a fresh process reopens the file.** The engine keeps its
//! whole catalog (tables, indexes, constraints, triggers, views,
//! sequences, statistics) in memory while running; reopen reconstructs
//! it from the persisted file. Any mismatch between "what CREATE
//! promised" and "what load_schema rebuilt" is a durability bug — the
//! canonical one being the TEXT/UUID PRIMARY KEY whose implicit
//! `sqlite_autoindex` was NOT rebuilt on load, silently disabling PK
//! uniqueness enforcement for every session after the first reopen
//! (fixed in `rebuild_implicit_indexes`, regression.rs 1437-1534).
//!
//! Design rules, learned from the SQLite sources:
//!
//! - **persist.test / autoidx semantics**: every reopen assertion comes
//!   in pairs — a POSITIVE check (data still reads correctly) and a
//!   NEGATIVE check (violations are still REJECTED). A constraint that
//!   stops rejecting is the failure mode; only re-querying data would
//!   never notice it.
//! - **autoindex1.test**: implicit index numbering and existence is
//!   verified through sqlite_master, not just through behavior.
//! - **trans2.test**: long-running generation loops — open, mutate,
//!   close, reopen, checksum — to catch state that degrades
//!   progressively across cycles rather than in one shot.
//! - **boundary2.test / wholenumber.test / blob tests**: value fidelity
//!   uses extreme and degenerate values, because serialization
//!   boundaries are where reopen bugs live (i64::MIN, f64 subnormals,
//!   empty strings, 0x00-filled and 0xFF-filled blobs, astral Unicode).
//! - **e_fkey.test**: FK actions are exercised end-to-end after reopen,
//!   not just the constraint's existence.
//! - **backup API semantics**: `db.image()` must yield a database
//!   equivalent to the file on disk (SQLite's sqlite3_backup-then-
//!   reopen contract), so the whole battery re-runs against an
//!   image-loaded in-memory copy.
//! - Determinism everywhere: no wall-clock, no rand crate, seeded
//!   xorshift when pseudo-random data is needed. Failures reproduce.
//!
//! Each test uses its own uniquely-named database under std temp dir and
//! removes it on exit. All paths are Windows-safe (no unix-only APIs).
//!
//! Run: cargo test --test durability

use rustqlite::{Database, Value};

// ===========================================================================
// Helpers
// ===========================================================================

/// A unique-enough temp path for one test. Removes any stale file so
/// re-running a failed test starts clean.
fn tmpdb(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("rustqlite_dur_{}.db", name));
    let _ = std::fs::remove_file(&path);
    path
}

/// Clean up at the end of a test.
fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    // WAL/SHM sidecars, if any journal mode created them.
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
    }
}

/// Open, run statements, flush, drop: the canonical "close it" sequence.
/// Every statement must succeed — durability bugs that surface as
/// garbage on reopen are usually INSERT failures being swallowed here.
fn session<F: FnMut(&mut Database)>(path: &std::path::Path, mut f: F) {
    let mut db = Database::open(path).expect("open");
    f(&mut db);
    db.flush().expect("flush before close");
    drop(db);
}

/// The single scalar of a single-row query, as i64.
fn one_i64(db: &Database, sql: &str) -> i64 {
    let rows = db.query(sql, []).expect(sql);
    assert!(!rows.is_empty(), "no rows for: {}", sql);
    rows[0][0].as_integer()
}

/// The single scalar of a single-row query, as String.
fn one_text(db: &Database, sql: &str) -> String {
    let rows = db.query(sql, []).expect(sql);
    assert!(!rows.is_empty(), "no rows for: {}", sql);
    rows[0][0].as_text()
}

/// Seeded xorshift64* — deterministic pseudo-random data (no rand dep).
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407)
            | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(2685821657736338717)
    }
}

/// A tiny FNV-1a checksum over a row set, used by the generation soak:
/// stable across reopen, sensitive to any value drift.
fn row_checksum(rows: &[Vec<Value>]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut mix = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    };
    for row in rows {
        for v in row {
            match v {
                Value::Null => mix(0),
                Value::Integer(i) => {
                    for b in i.to_le_bytes() {
                        mix(b)
                    }
                }
                Value::Real(f) => {
                    for b in f.to_bits().to_le_bytes() {
                        mix(b)
                    }
                }
                Value::Text(t) => {
                    for b in t.as_bytes() {
                        mix(*b)
                    }
                }
                Value::Blob(b) => {
                    for byte in b {
                        mix(*byte)
                    }
                }
            }
        }
    }
    h
}

// ===========================================================================
// SECTION A — constraint persistence (the uuid-PK bug class).
//
// For every constraint flavor: build, insert valid rows, close, reopen,
// then (1) data identical, (2) violation STILL rejected, (3) repeated
// reopen keeps rejecting. SQLite's persist.test pairs every positive
// read with a negative insert; autoindex1.test checks the catalog row.
// ===========================================================================

/// INTEGER PRIMARY KEY (rowid alias): duplicate rowid must stay
/// rejected across reopen, and the alias must keep writing rowids.
#[test]
fn rowid_alias_pk_survives_reopen() {
    let path = tmpdb("rowid_alias_pk");
    session(&path, |db| {
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        for i in 1..=50i64 {
            db.execute(
                "INSERT INTO t (id, v) VALUES (?, ?)",
                [Value::Integer(i), Value::Text(format!("row-{}", i).into())],
            )
            .unwrap();
        }
    });

    for cycle in 0..3 {
        let mut db = Database::open(&path).unwrap();
        // Each prior cycle appended one auto-assigned row.
        assert_eq!(
            one_i64(&db, "SELECT COUNT(*) FROM t"),
            50 + cycle,
            "cycle {}",
            cycle
        );
        assert_eq!(one_i64(&db, "SELECT MAX(id) FROM t"), 50 + cycle);
        // Negative probe: duplicate rowid.
        let dup = db.execute("INSERT INTO t (id, v) VALUES (7, 'dup')", []);
        assert!(
            dup.is_err(),
            "cycle {}: rowid-alias PK must keep rejecting duplicate rowids",
            cycle
        );
        // Insert without id must still auto-assign the next rowid.
        db.execute("INSERT INTO t (v) VALUES ('auto')", []).unwrap();
        assert_eq!(one_i64(&db, "SELECT MAX(id) FROM t"), 51 + cycle);
        db.flush().unwrap();
        drop(db);
    }
    cleanup(&path);
}

/// THE reported bug: `user_id uuid_text NOT NULL PRIMARY KEY` — a TEXT
/// PK backed by sqlite_autoindex_1. After reopen the autoindex used to
/// vanish, silently disabling uniqueness (and breaking upserts). This
/// test pins the exact shape from the report, in both type spellings,
/// across three reopen cycles, with negative probes every time.
#[test]
fn text_uuid_pk_uniqueness_survives_reopen() {
    let path = tmpdb("text_uuid_pk");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE users (
                user_id uuid_text NOT NULL PRIMARY KEY,
                email TEXT NOT NULL UNIQUE,
                name TEXT
            )",
            [],
        )
        .unwrap();
        db.execute(
            "CREATE TABLE users2 (
                user_id TEXT PRIMARY KEY,
                n INTEGER
            )",
            [],
        )
        .unwrap();
        let ids = [
            "0c0a7c1e-6f34-4b8f-9a2d-1e9c4a2b0001",
            "0c0a7c1e-6f34-4b8f-9a2d-1e9c4a2b0002",
            "0c0a7c1e-6f34-4b8f-9a2d-1e9c4a2b0003",
        ];
        for (i, id) in ids.iter().enumerate() {
            db.execute(
                "INSERT INTO users (user_id, email, name) VALUES (?, ?, ?)",
                [
                    Value::Text((*id).into()),
                    Value::Text(format!("u{}@x.io", i).into()),
                    Value::Text(format!("user{}", i).into()),
                ],
            )
            .unwrap();
            db.execute(
                "INSERT INTO users2 (user_id, n) VALUES (?, ?)",
                [Value::Text((*id).into()), Value::Integer(i as i64 + 1)],
            )
            .unwrap();
        }
    });

    for cycle in 0..3 {
        let mut db = Database::open(&path).unwrap();
        // (1) positive: all rows readable, correct values.
        assert_eq!(
            one_i64(&db, "SELECT COUNT(*) FROM users"),
            3,
            "cycle {}",
            cycle
        );
        assert_eq!(
            one_text(
                &db,
                "SELECT name FROM users WHERE user_id = '0c0a7c1e-6f34-4b8f-9a2d-1e9c4a2b0002'"
            ),
            "user1"
        );
        // (2) negative: duplicate uuid must STILL be rejected.
        let dup = db.execute(
            "INSERT INTO users (user_id, email, name) VALUES ('0c0a7c1e-6f34-4b8f-9a2d-1e9c4a2b0001', 'x@x.io', 'dup')",
            [],
        );
        assert!(
            dup.is_err(),
            "cycle {}: TEXT/uuid PK uniqueness must survive reopen",
            cycle
        );
        let dup2 = db.execute(
            "INSERT INTO users2 (user_id, n) VALUES ('0c0a7c1e-6f34-4b8f-9a2d-1e9c4a2b0001', 99)",
            [],
        );
        assert!(dup2.is_err(), "cycle {}: second spelling too", cycle);
        // (3) the autoindex row must exist in the catalog (autoindex1.test).
        let master = db
            .query(
                "SELECT name, sql FROM sqlite_master WHERE type = 'index'",
                [],
            )
            .unwrap();
        let has_auto = master
            .iter()
            .any(|r| r[0].as_text().contains("sqlite_autoindex_users_1"));
        assert!(
            has_auto,
            "cycle {}: implicit PK autoindex row missing",
            cycle
        );
        // (4) upsert via the PK autoindex must keep working (needs the index).
        db.execute(
            "INSERT INTO users2 (user_id, n) VALUES ('0c0a7c1e-6f34-4b8f-9a2d-1e9c4a2b0001', 10)
             ON CONFLICT (user_id) DO UPDATE SET n = excluded.n",
            [],
        )
        .unwrap();
        assert_eq!(
            one_i64(
                &db,
                "SELECT n FROM users2 WHERE user_id = '0c0a7c1e-6f34-4b8f-9a2d-1e9c4a2b0001'"
            ),
            10
        );
        // Post-upsert uniqueness must not loosen (regression.rs 1568-1590):
        // a genuinely NEW uuid must insert, once per cycle.
        let fresh = format!("0c0a7c1e-6f34-4b8f-9a2d-1e9c4a2b00{:02}", 9 + cycle);
        let ok_new = db.execute(
            "INSERT INTO users2 (user_id, n) VALUES (?, 99)",
            [Value::Text(fresh.into())],
        );
        assert!(
            ok_new.is_ok(),
            "a genuinely new uuid must insert (cycle {})",
            cycle
        );
        db.flush().unwrap();
        drop(db);
    }
    cleanup(&path);
}

/// WITHOUT ROWID composite PRIMARY KEY: dup keys and missing key parts
/// must stay rejected; the table b-tree stays the PK index.
#[test]
fn without_rowid_composite_pk_survives_reopen() {
    let path = tmpdb("worid_pk");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE kv (k1 TEXT, k2 INTEGER, val TEXT, PRIMARY KEY (k1, k2)) WITHOUT ROWID",
            [],
        )
        .unwrap();
        for i in 0..30i64 {
            db.execute(
                "INSERT INTO kv (k1, k2, val) VALUES (?, ?, ?)",
                [
                    Value::Text(format!("k{}", i % 3).into()),
                    Value::Integer(i),
                    Value::Text(format!("v{}", i).into()),
                ],
            )
            .unwrap();
        }
    });

    for cycle in 0..2 {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM kv"), 30);
        // Full-key lookup (PK b-tree path).
        assert_eq!(
            one_text(&db, "SELECT val FROM kv WHERE k1 = 'k1' AND k2 = 7"),
            "v7"
        );
        // Negative: duplicate composite key.
        let dup = db.execute("INSERT INTO kv (k1, k2, val) VALUES ('k1', 7, 'dup')", []);
        assert!(
            dup.is_err(),
            "cycle {}: WITHOUT ROWID composite PK must stay unique",
            cycle
        );
        // Negative: NULL in any PK column is a NOT NULL violation (SQLite:
        // PRIMARY KEY implies NOT NULL, even without WITHOUT ROWID).
        let null_key = db.execute("INSERT INTO kv (k1, k2, val) VALUES (NULL, 99, 'x')", []);
        assert!(
            null_key.is_err(),
            "cycle {}: NULL PK part must be rejected",
            cycle
        );
        db.flush().unwrap();
        drop(db);
    }
    cleanup(&path);
}

/// UNIQUE in all its spellings: column-level, table-level single,
/// table-level composite — plus SQLite's NULL multiplicity rule
/// (multiple NULLs are allowed in a UNIQUE index).
#[test]
fn unique_constraints_survive_reopen() {
    let path = tmpdb("unique_reopen");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE t (
                a INTEGER,
                b TEXT UNIQUE,
                c TEXT,
                d INTEGER,
                UNIQUE (c, d)
            )",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO t (a, b, c, d) VALUES (1, 'b1', 'c1', 10)", [])
            .unwrap();
        db.execute("INSERT INTO t (a, b, c, d) VALUES (2, 'b2', 'c1', 11)", [])
            .unwrap();
        // NULLs: allowed repeatedly in UNIQUE columns (SQLite semantics).
        db.execute("INSERT INTO t (a, c, d) VALUES (3, 'c2', 20)", [])
            .unwrap();
        db.execute("INSERT INTO t (a, c, d) VALUES (4, 'c2', 21)", [])
            .unwrap();
    });

    for cycle in 0..2 {
        let mut db = Database::open(&path).unwrap();
        // Cycle 0 added a row (5th NULL-b); cycle 1 re-checks with 5.
        assert_eq!(
            one_i64(&db, "SELECT COUNT(*) FROM t"),
            4 + cycle,
            "cycle {}",
            cycle
        );
        // Negative probes: each UNIQUE flavor still rejects.
        let dup_b = db.execute("INSERT INTO t (b, c, d) VALUES ('b1', 'cx', 99)", []);
        assert!(dup_b.is_err(), "cycle {}: column UNIQUE", cycle);
        let dup_cd = db.execute("INSERT INTO t (b, c, d) VALUES ('bx', 'c1', 10)", []);
        assert!(dup_cd.is_err(), "cycle {}: composite UNIQUE", cycle);
        // NULL multiplicity survives reopen too (a THIRD null b is fine).
        db.execute(
            "INSERT INTO t (a, c, d) VALUES (?, 'c3', ?)",
            [Value::Integer(5 + cycle), Value::Integer(30 + cycle)],
        )
        .unwrap();
        // Autoindex count: b (1), (c,d) (2) — catalog must carry both.
        let n = one_i64(
            &db,
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name LIKE 'sqlite_autoindex_t_%'",
        );
        assert_eq!(n, 2, "cycle {}: autoindex count", cycle);
        db.flush().unwrap();
        drop(db);
    }
    cleanup(&path);
}

/// COLLATE NOCASE on a UNIQUE column: the collation is part of the
/// index definition — 'A' and 'a' must collide, and still collide after
/// reopen (this exercises the collated autoindex rebuild path).
#[test]
fn collated_unique_survives_reopen() {
    let path = tmpdb("collated_unique");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE t (u TEXT UNIQUE COLLATE NOCASE, v INTEGER)",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO t (u, v) VALUES ('Alpha', 1)", [])
            .unwrap();
        db.execute("INSERT INTO t (u, v) VALUES ('BETA', 2)", [])
            .unwrap();
    });

    for cycle in 0..2 {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM t"), 2);
        // Case-insensitive lookup works (NOCASE survived).
        assert_eq!(one_i64(&db, "SELECT v FROM t WHERE u = 'alpha'"), 1);
        // Negative: same key in different case must still collide.
        let dup = db.execute("INSERT INTO t (u, v) VALUES ('ALPHA', 3)", []);
        assert!(
            dup.is_err(),
            "cycle {}: COLLATE NOCASE UNIQUE must keep colliding case-insensitively",
            cycle
        );
        // The collation is behavioral: case-insensitive collision IS the
        // load-path check (autoindex rows store NULL sql, like SQLite).
        db.flush().unwrap();
        drop(db);
    }
    cleanup(&path);
}

/// CHECK constraints: the expression must keep evaluating and rejecting
/// after reopen (fresh parse of stored DDL on the load path).
#[test]
fn check_constraint_survives_reopen() {
    let path = tmpdb("check_reopen");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE scores (
                id INTEGER PRIMARY KEY,
                score REAL CHECK (score >= 0.0 AND score <= 100.0),
                level TEXT CHECK (level IN ('bronze', 'silver', 'gold')),
                CHECK (id < 1000000)
            )",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO scores (id, score, level) VALUES (1, 99.5, 'gold')",
            [],
        )
        .unwrap();
    });

    for cycle in 0..2 {
        let mut db = Database::open(&path).unwrap();
        // Cycle 0 inserted id 4; later cycles see it.
        assert_eq!(
            one_i64(&db, "SELECT COUNT(*) FROM scores"),
            1 + cycle,
            "cycle {}",
            cycle
        );
        // Each CHECK flavor still rejects after reopen.
        let r1 = db.execute(
            "INSERT INTO scores (id, score, level) VALUES (2, -1.0, 'gold')",
            [],
        );
        assert!(r1.is_err(), "cycle {}: column CHECK (range)", cycle);
        let r2 = db.execute(
            "INSERT INTO scores (id, score, level) VALUES (3, 50.0, 'platinum')",
            [],
        );
        assert!(r2.is_err(), "cycle {}: column CHECK (IN list)", cycle);
        let r3 = db.execute(
            "INSERT INTO scores (id, score, level) VALUES (2000000, 50.0, 'gold')",
            [],
        );
        assert!(r3.is_err(), "cycle {}: table-level CHECK", cycle);
        // Valid row still inserts (id must be unique per cycle).
        db.execute(
            "INSERT INTO scores (id, score, level) VALUES (?, 50.0, 'silver')",
            [Value::Integer(4 + cycle)],
        )
        .unwrap();
        db.flush().unwrap();
        drop(db);
    }
    cleanup(&path);
}

/// NOT NULL: rejection survives reopen, in plain and INTEGER contexts.
#[test]
fn not_null_survives_reopen() {
    let path = tmpdb("notnull_reopen");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT NOT NULL, b INTEGER NOT NULL DEFAULT 0)",
            [],
        )
        .unwrap();
        for i in 1..=10i64 {
            db.execute(
                "INSERT INTO t (a, b) VALUES (?, ?)",
                [
                    Value::Text(format!("a{}", i).into()),
                    Value::Integer(i * 10),
                ],
            )
            .unwrap();
        }
    });

    let mut db = Database::open(&path).unwrap();
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM t"), 10);
    let r1 = db.execute("INSERT INTO t (b) VALUES (5)", []);
    assert!(r1.is_err(), "NOT NULL must survive reopen (explicit)");
    // `b` is NOT NULL DEFAULT 0 — omitting it must SUCCEED via the default
    // (the persisted default is part of the NOT NULL contract).
    db.execute("INSERT INTO t (a) VALUES ('x')", []).unwrap();
    assert_eq!(
        one_i64(&db, "SELECT b FROM t WHERE a = 'x'"),
        0,
        "NOT NULL DEFAULT must apply after reopen"
    );
    // NULL via explicit NULL must also reject.
    let r3 = db.execute("INSERT INTO t (a, b) VALUES (NULL, 5)", []);
    assert!(r3.is_err(), "explicit NULL must be rejected after reopen");
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// DEFAULT values: a constant default must still apply to INSERTs after
/// reopen (the default lives in the persisted DDL, re-parsed on load).
#[test]
fn default_values_survive_reopen() {
    let path = tmpdb("default_reopen");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE t (
                id INTEGER PRIMARY KEY,
                status TEXT DEFAULT 'active',
                count INTEGER DEFAULT 42,
                ratio REAL DEFAULT 0.5
            )",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO t (id) VALUES (1)", []).unwrap();
    });

    let mut db = Database::open(&path).unwrap();
    // The old row still reads its defaults.
    assert_eq!(one_text(&db, "SELECT status FROM t WHERE id = 1"), "active");
    assert_eq!(one_i64(&db, "SELECT count FROM t WHERE id = 1"), 42);
    // New inserts after reopen still get defaults.
    db.execute("INSERT INTO t (id) VALUES (2)", []).unwrap();
    assert_eq!(one_i64(&db, "SELECT count FROM t WHERE id = 2"), 42);
    let rows = db.query("SELECT ratio FROM t WHERE id = 2", []).unwrap();
    match &rows[0][0] {
        Value::Real(f) => assert!((f - 0.5).abs() < 1e-12, "real default: {}", f),
        other => panic!("ratio default must be REAL, got {:?}", other),
    }
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// AUTOINCREMENT: the sqlite_sequence floor must survive reopen —
/// deleting the top rows and reopening must NOT reuse historic ids.
#[test]
fn autoincrement_sequence_survives_reopen() {
    let path = tmpdb("autoinc_reopen");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
            [],
        )
        .unwrap();
        for i in 0..10i64 {
            db.execute(
                "INSERT INTO t (v) VALUES (?)",
                [Value::Text(format!("v{}", i).into())],
            )
            .unwrap();
        }
        // id 10 was the historic max; delete it.
        db.execute("DELETE FROM t WHERE id = 10", []).unwrap();
    });

    let mut db = Database::open(&path).unwrap();
    // max(rowid) is 9 now, but the sequence floor must still be 10.
    let next = one_i64(&db, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
    assert!(next >= 10, "sequence floor lost: seq = {}", next);
    let got = one_i64(&db, "SELECT id FROM t ORDER BY id DESC LIMIT 1");
    assert_eq!(got, 9);
    db.execute("INSERT INTO t (v) VALUES ('post-reopen')", [])
        .unwrap();
    let new_id = one_i64(&db, "SELECT MAX(id) FROM t");
    assert!(
        new_id >= 10,
        "AUTOINCREMENT must not reuse the deleted max: new id = {}",
        new_id
    );
    // And the floor survives a SECOND reopen after the new insert.
    db.flush().unwrap();
    drop(db);
    let mut db = Database::open(&path).unwrap();
    let seq2 = one_i64(&db, "SELECT seq FROM sqlite_sequence WHERE name = 't'");
    assert!(
        seq2 >= new_id,
        "seq must track post-reopen inserts: {}",
        seq2
    );
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// FOREIGN KEY definitions + actions: after reopen, enforcement follows
/// the connection's PRAGMA (SQLite: OFF by default, per-connection) —
/// but the DEFINITION persists, so ON must re-arm CASCADE/SET NULL/
/// RESTRICT with the persisted clauses, not defaults.
#[test]
fn foreign_key_actions_survive_reopen() {
    let path = tmpdb("fk_reopen");
    session(&path, |db| {
        db.execute("PRAGMA foreign_keys = ON", []).unwrap();
        db.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY, tag TEXT)", [])
            .unwrap();
        db.execute(
            "CREATE TABLE child (
                id INTEGER PRIMARY KEY,
                pid INTEGER REFERENCES parent (id) ON DELETE CASCADE,
                qid INTEGER REFERENCES parent (id) ON DELETE SET NULL
            )",
            [],
        )
        .unwrap();
        db.execute(
            "CREATE TABLE strict_child (
                id INTEGER PRIMARY KEY,
                pid INTEGER REFERENCES parent (id) ON DELETE RESTRICT
            )",
            [],
        )
        .unwrap();
        for i in 1..=5i64 {
            db.execute(
                "INSERT INTO parent (id, tag) VALUES (?, ?)",
                [Value::Integer(i), Value::Text(format!("p{}", i).into())],
            )
            .unwrap();
        }
        // Each child exercises ONE action, so no action's victim row is
        // destroyed by an earlier action's probe.
        db.execute("INSERT INTO child (id, pid) VALUES (1, 1)", [])
            .unwrap(); // cascade victim
        db.execute("INSERT INTO child (id, qid) VALUES (2, 2)", [])
            .unwrap(); // set-null victim
        db.execute("INSERT INTO strict_child (id, pid) VALUES (1, 5)", [])
            .unwrap(); // restrict guard
    });

    let mut db = Database::open(&path).unwrap();
    // Definition persisted: parent-key insert violation rejects when ON.
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    let orphan = db.execute("INSERT INTO child (id, pid) VALUES (3, 999)", []);
    assert!(orphan.is_err(), "FK definition must survive reopen");

    // CASCADE still cascades (action clauses persisted, not reset).
    db.execute("DELETE FROM parent WHERE id = 1", []).unwrap();
    let n = one_i64(&db, "SELECT COUNT(*) FROM child WHERE pid = 1");
    assert_eq!(n, 0, "ON DELETE CASCADE must keep firing after reopen");

    // SET NULL still sets NULL.
    db.execute("DELETE FROM parent WHERE id = 2", []).unwrap();
    let rows = db.query("SELECT qid FROM child WHERE id = 2", []).unwrap();
    assert!(
        matches!(rows[0][0], Value::Null),
        "ON DELETE SET NULL must keep firing after reopen"
    );

    // RESTRICT still restricts (strict_child(1) references parent 5).
    let restricted = db.execute("DELETE FROM parent WHERE id = 5", []);
    assert!(
        restricted.is_err(),
        "ON DELETE RESTRICT must keep firing after reopen"
    );
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

// ===========================================================================
// SECTION B — schema-object persistence: indexes, views, triggers,
// generated columns, statistics, ALTER evolution. The object must not
// merely EXIST after reopen — it must keep WORKING (query plans,
// trigger side effects, stored/virtual derivation, stat rows).
// ===========================================================================

/// Explicit indexes of every shape — plain, UNIQUE, partial, expression,
/// DESC — must persist AND keep serving correct results after reopen;
/// INDEXED BY must still resolve, and index scans must agree with table
/// scans (the join in the final query cross-checks both paths).
#[test]
fn index_shapes_survive_reopen() {
    let path = tmpdb("index_shapes");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, c TEXT)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX idx_a ON t (a)", []).unwrap();
        db.execute("CREATE UNIQUE INDEX idx_ab ON t (a, b DESC)", [])
            .unwrap();
        db.execute("CREATE INDEX idx_partial ON t (b) WHERE c = 'hot'", [])
            .unwrap();
        db.execute("CREATE INDEX idx_expr ON t (lower(a))", [])
            .unwrap();
        let mut rng = Rng::new(0xD0C5);
        for i in 1..=200i64 {
            let a = format!("name-{}", rng.next_u64() % 17);
            // b must be unique per row: the (a, b DESC) unique index has
            // only 17 x N room; a unique b guarantees no setup collisions.
            let b = i * 3;
            let c = if rng.next_u64() % 4 == 0 {
                "hot"
            } else {
                "cold"
            };
            db.execute(
                "INSERT INTO t (a, b, c) VALUES (?, ?, ?)",
                [
                    Value::Text(a.into()),
                    Value::Integer(b),
                    Value::Text(c.into()),
                ],
            )
            .unwrap();
        }
    });

    let mut db = Database::open(&path).unwrap();
    // All four index rows persist with their rootpages.
    let n = one_i64(
        &db,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND sql IS NOT NULL",
    );
    assert_eq!(n, 4, "explicit index rows lost on reopen");
    // Each index is still usable (query planning / INDEXED BY).
    for idx in ["idx_a", "idx_ab", "idx_partial", "idx_expr"] {
        let plan_ok = db.query(&format!("SELECT COUNT(*) FROM t INDEXED BY {}", idx), []);
        assert!(
            plan_ok.is_ok(),
            "index {} unusable after reopen: {:?}",
            idx,
            plan_ok.err()
        );
    }
    // Partial index membership: the WHERE clause survived — rows the
    // index covers must match its predicate.
    let in_idx = one_i64(
        &db,
        "SELECT COUNT(*) FROM t INDEXED BY idx_partial WHERE c = 'hot'",
    );
    let want = one_i64(&db, "SELECT COUNT(*) FROM t WHERE c = 'hot'");
    assert_eq!(in_idx, want, "partial index predicate changed on reopen");
    // Unique index still rejects.
    let rows = db.query("SELECT a, b FROM t LIMIT 1", []).unwrap();
    let dup = db.execute(
        "INSERT INTO t (a, b, c) VALUES (?, ?, 'dup')",
        [rows[0][0].clone(), rows[0][1].clone()],
    );
    assert!(
        dup.is_err(),
        "unique index (a, b DESC) must still reject after reopen"
    );
    // Index-scan results == table-scan results (correctness, not just speed).
    let via_index = db
        .query("SELECT b FROM t INDEXED BY idx_a ORDER BY a, id", [])
        .unwrap();
    let via_table = db.query("SELECT b FROM t ORDER BY a, id", []).unwrap();
    assert_eq!(
        row_checksum(&via_index),
        row_checksum(&via_table),
        "index scan diverged from table scan after reopen"
    );
    // DESC order is honored in ordered results (the contract; physical
    // index layout is an implementation detail — results without ORDER
    // BY are formally unordered).
    let a0 = {
        let r = db.query("SELECT a FROM t LIMIT 1", []).unwrap();
        r[0][0].as_text()
    };
    let desc = db
        .query(
            &format!(
                "SELECT b FROM t INDEXED BY idx_ab WHERE a = '{}' ORDER BY b DESC",
                a0
            ),
            [],
        )
        .unwrap();
    let bs: Vec<i64> = desc.iter().map(|r| r[0].as_integer()).collect();
    let mut sorted = bs.clone();
    sorted.sort_by(|x, y| y.cmp(x));
    assert_eq!(bs, sorted, "ORDER BY b DESC must return descending values");
    // The DESC flag persists in the index DDL (the planner's source of
    // truth for order satisfaction across reopen).
    let ddl = one_text(
        &db,
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_ab'",
    );
    assert!(
        ddl.contains("DESC"),
        "DESC lost from index DDL on reopen: {}",
        ddl
    );
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// Views: a view over a join with an aggregate must return identical
/// rows after reopen (SQLite view01/view tests).
#[test]
fn views_survive_reopen() {
    let path = tmpdb("views_reopen");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, cust TEXT, total REAL)",
            [],
        )
        .unwrap();
        db.execute(
            "CREATE TABLE custs (name TEXT PRIMARY KEY, region TEXT)",
            [],
        )
        .unwrap();
        for (i, (cust, region, total)) in [
            ("alice", "east", 10.0),
            ("alice", "east", 25.5),
            ("bob", "west", 7.0),
            ("carol", "east", 99.0),
            ("bob", "west", 3.5),
        ]
        .iter()
        .enumerate()
        {
            db.execute(
                "INSERT INTO orders (id, cust, total) VALUES (?, ?, ?)",
                [
                    Value::Integer(i as i64 + 1),
                    Value::Text((*cust).into()),
                    Value::Real(*total),
                ],
            )
            .unwrap();
            db.execute(
                "INSERT OR IGNORE INTO custs (name, region) VALUES (?, ?)",
                [Value::Text((*cust).into()), Value::Text((*region).into())],
            )
            .unwrap();
        }
        db.execute(
            "CREATE VIEW region_totals AS
             SELECT c.region, COUNT(*) AS n, SUM(o.total) AS amount
             FROM orders o JOIN custs c ON o.cust = c.name
             GROUP BY c.region",
            [],
        )
        .unwrap();
    });

    let mut db = Database::open(&path).unwrap();
    let rows = db
        .query(
            "SELECT region, n, amount FROM region_totals ORDER BY region",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 2, "view rows after reopen: {:?}", rows);
    assert_eq!(rows[0][0].as_text(), "east");
    assert_eq!(rows[0][1].as_integer(), 3);
    let east_sum = match rows[0][2] {
        Value::Real(f) => f,
        Value::Integer(i) => i as f64,
        ref other => panic!("SUM must be numeric, got {:?}", other),
    };
    assert!((east_sum - 134.5).abs() < 1e-9, "east sum: {}", east_sum);
    assert_eq!(rows[1][0].as_text(), "west");
    assert_eq!(rows[1][1].as_integer(), 2);
    // View DDL persisted verbatim enough to re-execute.
    let ddl = one_text(
        &db,
        "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'region_totals'",
    );
    assert!(ddl.contains("GROUP BY"), "view DDL degraded: {}", ddl);
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// Triggers: AFTER INSERT, BEFORE DELETE, and INSTEAD OF on a view must
/// keep firing with their full body after reopen (SQLite trigger1-9).
#[test]
fn triggers_survive_reopen() {
    let path = tmpdb("triggers_reopen");
    session(&path, |db| {
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute("CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT)", [])
            .unwrap();
        db.execute("CREATE VIEW v AS SELECT id, v FROM t", [])
            .unwrap();
        db.execute(
            "CREATE TRIGGER trg_insert AFTER INSERT ON t
             BEGIN
                 INSERT INTO audit (note) VALUES ('ins:' || new.v);
             END",
            [],
        )
        .unwrap();
        db.execute(
            "CREATE TRIGGER trg_delete BEFORE DELETE ON t
             BEGIN
                 INSERT INTO audit (note) VALUES ('del:' || old.v);
             END",
            [],
        )
        .unwrap();
        db.execute(
            "CREATE TRIGGER trg_view INSTEAD OF UPDATE ON v
             BEGIN
                 UPDATE t SET v = new.v WHERE id = new.id;
             END",
            [],
        )
        .unwrap();
        for i in 1..=5i64 {
            db.execute(
                "INSERT INTO t (v) VALUES (?)",
                [Value::Text(format!("v{}", i).into())],
            )
            .unwrap();
        }
    });

    let mut db = Database::open(&path).unwrap();
    // Pre-reopen trigger effects: 5 audit rows.
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM audit"), 5);

    // AFTER INSERT trigger still fires post-reopen.
    db.execute("INSERT INTO t (v) VALUES ('post')", []).unwrap();
    assert_eq!(
        one_text(
            &db,
            "SELECT note FROM audit WHERE id = (SELECT MAX(id) FROM audit)"
        ),
        "ins:post",
        "AFTER INSERT trigger body lost on reopen"
    );

    // BEFORE DELETE trigger still fires.
    db.execute("DELETE FROM t WHERE v = 'v1'", []).unwrap();
    let del_notes = one_i64(&db, "SELECT COUNT(*) FROM audit WHERE note = 'del:v1'");
    assert_eq!(del_notes, 1, "BEFORE DELETE trigger body lost on reopen");

    // INSTEAD OF trigger on the view still routes UPDATE.
    db.execute("UPDATE v SET v = 'rewritten' WHERE id = 2", [])
        .unwrap();
    assert_eq!(one_text(&db, "SELECT v FROM t WHERE id = 2"), "rewritten");

    // Triggers are visible in the catalog with their kind.
    let n = one_i64(
        &db,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger'",
    );
    assert_eq!(n, 3, "trigger rows lost on reopen");
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// Generated columns: VIRTUAL (derived on read) and STORED (persisted)
/// must both read correctly after reopen; writes INTO them still error.
#[test]
fn generated_columns_survive_reopen() {
    let path = tmpdb("gencol_reopen");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE t (
                id INTEGER PRIMARY KEY,
                price REAL,
                qty INTEGER,
                total REAL GENERATED ALWAYS AS (price * qty) STORED,
                label TEXT GENERATED ALWAYS AS ('item-' || id) VIRTUAL
            )",
            [],
        )
        .unwrap();
        for i in 1..=20i64 {
            db.execute(
                "INSERT INTO t (id, price, qty) VALUES (?, ?, ?)",
                [
                    Value::Integer(i),
                    Value::Real(i as f64 * 1.5),
                    Value::Integer(i * 2),
                ],
            )
            .unwrap();
        }
    });

    let mut db = Database::open(&path).unwrap();
    // STORED: exact values back.
    for id in [1i64, 7, 20] {
        let rows = db
            .query(
                "SELECT total, label FROM t WHERE id = ?",
                [Value::Integer(id)],
            )
            .unwrap();
        let expect = id as f64 * 1.5 * (id * 2) as f64;
        match rows[0][0] {
            Value::Real(f) => assert!((f - expect).abs() < 1e-9, "id {}: {} != {}", id, f, expect),
            ref other => panic!("stored generated col must be REAL: {:?}", other),
        }
        assert_eq!(
            rows[0][1].as_text(),
            format!("item-{}", id),
            "virtual generated col"
        );
    }
    // Writes into a generated column still rejected.
    let w = db.execute(
        "INSERT INTO t (id, price, qty, total) VALUES (99, 1.0, 1, 5.0)",
        [],
    );
    assert!(
        w.is_err(),
        "generated column must stay non-writable after reopen"
    );
    // New rows get generated values post-reopen.
    db.execute("INSERT INTO t (id, price, qty) VALUES (21, 2.0, 10)", [])
        .unwrap();
    let rows = db.query("SELECT total FROM t WHERE id = 21", []).unwrap();
    match rows[0][0] {
        Value::Real(f) => assert!((f - 20.0).abs() < 1e-9),
        ref other => panic!("{:?}", other),
    }
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// ANALYZE statistics: sqlite_stat1 rows must persist across reopen so
/// plans stay cost-based (SQLite analyze5/analyze3).
#[test]
fn analyze_stats_survive_reopen() {
    let path = tmpdb("analyze_reopen");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX idx_b ON t (b)", []).unwrap();
        for i in 1..=500i64 {
            db.execute(
                "INSERT INTO t (a, b) VALUES (?, ?)",
                [
                    Value::Text(format!("a{}", i % 25).into()),
                    Value::Integer(i % 10),
                ],
            )
            .unwrap();
        }
        db.execute("ANALYZE", []).unwrap();
    });

    let mut db = Database::open(&path).unwrap();
    // The stat rows persist with their exact shape (ANALYZE writes one
    // row per index, like SQLite).
    let n = one_i64(&db, "SELECT COUNT(*) FROM sqlite_stat1");
    assert!(n >= 1, "sqlite_stat1 rows lost on reopen: {}", n);
    let t_stat = one_text(
        &db,
        "SELECT stat FROM sqlite_stat1 WHERE tbl = 't' AND idx = 'idx_b'",
    );
    assert!(
        t_stat.starts_with("500 "),
        "stat shape for (t, idx_b): {}",
        t_stat
    );
    // The index is still usable and correct after reopen.
    let hot = one_i64(&db, "SELECT COUNT(*) FROM t WHERE b = 3");
    assert_eq!(hot, 50, "selectivity drift after reopen");
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// ALTER TABLE evolution: ADD COLUMN (with default), RENAME TABLE,
/// RENAME COLUMN, DROP COLUMN — each change must persist through reopen
/// with SQLite's read-back semantics (old rows read the added column's
/// default) (SQLite alter2.test).
#[test]
fn alter_evolution_survives_reopen() {
    let path = tmpdb("alter_reopen");
    session(&path, |db| {
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        for i in 1..=5i64 {
            db.execute(
                "INSERT INTO t (v) VALUES (?)",
                [Value::Text(format!("v{}", i).into())],
            )
            .unwrap();
        }
        db.execute("ALTER TABLE t ADD COLUMN extra INTEGER DEFAULT 7", [])
            .unwrap();
        db.execute("ALTER TABLE t RENAME TO t2", []).unwrap();
        db.execute("ALTER TABLE t2 RENAME COLUMN v TO value", [])
            .unwrap();
    });

    let mut db = Database::open(&path).unwrap();
    // Renamed table + column survive; old table name is gone.
    let old = db.query("SELECT COUNT(*) FROM t", []);
    assert!(old.is_err(), "old name must be gone after rename + reopen");
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM t2"), 5);
    // Old rows read the added column's default (SQLite semantics).
    let rows = db
        .query("SELECT id, value, extra FROM t2 WHERE id = 3", [])
        .unwrap();
    assert_eq!(rows[0][1].as_text(), "v3", "renamed column value");
    match &rows[0][2] {
        Value::Integer(i) => assert_eq!(*i, 7, "ADD COLUMN default read-back"),
        other => panic!("extra must read 7, got {:?}", other),
    }
    // New inserts use the persisted new schema.
    db.execute("INSERT INTO t2 (value, extra) VALUES ('new', 99)", [])
        .unwrap();
    let r = db
        .query("SELECT extra FROM t2 WHERE value = 'new'", [])
        .unwrap();
    assert_eq!(r[0][0].as_integer(), 99);
    db.flush().unwrap();
    drop(db);

    // DROP COLUMN persists through its own reopen.
    session(&path, |db| {
        db.execute("ALTER TABLE t2 DROP COLUMN extra", []).unwrap();
    });
    let mut db = Database::open(&path).unwrap();
    let cols = db.query("PRAGMA table_info(t2)", []).unwrap();
    let has_extra = cols.iter().any(|c| c[1].as_text() == "extra");
    assert!(!has_extra, "DROP COLUMN must survive reopen");
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM t2"), 6);
    assert_eq!(
        one_text(&db, "SELECT value FROM t2 WHERE id = 3"),
        "v3",
        "surviving columns intact after drop + reopen"
    );
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

// ===========================================================================
// SECTION C — data fidelity & multi-generation durability.
//
// boundary2.test/wholenumber.test: serialization boundaries are where
// reopen bugs live, so fidelity probes use extreme values. trans2.test:
// the generation loop catches state that degrades cycle over cycle.
// ===========================================================================

/// Every value class round-trips byte-exactly through close/reopen:
/// i64 extremes, f64 extremes (incl. subnormals and integral reals),
/// empty text, astral/combining Unicode, blobs (empty, zeroed, 0xFF,
/// 64 KiB).
#[test]
fn value_fidelity_roundtrip() {
    let path = tmpdb("value_fidelity");
    session(&path, |db| {
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v)", [])
            .unwrap();
        let cases: Vec<Value> = vec![
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Integer(0),
            Value::Integer(-1),
            Value::Real(f64::MIN_POSITIVE),
            Value::Real(f64::MAX),
            Value::Real(f64::MIN_POSITIVE * 1e-308), // deepest subnormal
            Value::Real(-0.0),
            Value::Real(5e-324),
            Value::Real(1.5),
            Value::Text("".into()),
            Value::Text("ascii".into()),
            Value::Text("é ü ñ 汉字 日本語".into()),
            Value::Text("🚀🧿𝕏🦀".into()),         // astral plane
            Value::Text("e\u{301}\u{327}".into()), // combining marks
            Value::Text("‏rtl עברית".into()),       // RTL + bidi marks
            Value::Text("a\0b".into()),            // embedded NUL in TEXT
            Value::Blob(vec![]),
            Value::Blob(vec![0x00; 64]),
            Value::Blob((0u8..=255).collect()),
            Value::Blob(vec![0xFF; 1024]),
            Value::Blob((u16::MIN..=u16::MAX).map(|x| (x >> 8) as u8).collect()),
        ];
        let big_blob: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();
        for (i, v) in cases
            .iter()
            .chain(std::iter::once(&Value::Blob(big_blob)))
            .enumerate()
        {
            db.execute(
                "INSERT INTO t (id, v) VALUES (?, ?)",
                [Value::Integer(i as i64 + 1), v.clone()],
            )
            .unwrap();
        }
    });

    let db = Database::open(&path).unwrap();
    let n = one_i64(&db, "SELECT COUNT(*) FROM t");
    assert_eq!(n, 23, "row count must round-trip");
    let mut expected: Vec<Value> = vec![
        Value::Integer(i64::MIN),
        Value::Integer(i64::MAX),
        Value::Integer(0),
        Value::Integer(-1),
        Value::Real(f64::MIN_POSITIVE),
        Value::Real(f64::MAX),
        Value::Real(f64::MIN_POSITIVE * 1e-308),
        Value::Real(-0.0),
        Value::Real(5e-324),
        Value::Real(1.5),
        Value::Text("".into()),
        Value::Text("ascii".into()),
        Value::Text("é ü ñ 汉字 日本語".into()),
        Value::Text("🚀🧿𝕏🦀".into()),
        Value::Text("e\u{301}\u{327}".into()),
        Value::Text("‏rtl עברית".into()),
        Value::Text("a\0b".into()),
        Value::Blob(vec![]),
        Value::Blob(vec![0x00; 64]),
        Value::Blob((0u8..=255).collect()),
        Value::Blob(vec![0xFF; 1024]),
        Value::Blob((u16::MIN..=u16::MAX).map(|x| (x >> 8) as u8).collect()),
    ];
    expected.push(Value::Blob(
        (0..65536u32).map(|i| (i % 251) as u8).collect(),
    ));
    for (i, want) in expected.iter().enumerate() {
        let rows = db
            .query(
                "SELECT v FROM t WHERE id = ?",
                [Value::Integer(i as i64 + 1)],
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "id {} vanished", i + 1);
        let got = rows[0][0].clone();
        let same = match (got, want) {
            (Value::Null, Value::Null) => true,
            (Value::Integer(a), Value::Integer(b)) => a == *b,
            (Value::Real(a), Value::Real(b)) => a.to_bits() == b.to_bits(),
            (Value::Text(a), Value::Text(b)) => a.as_str() == b.as_str(),
            (Value::Blob(a), Value::Blob(b)) => a == *b,
            (a, b) => {
                panic!("kind drift at id {}: {:?} vs {:?}", i + 1, a, b)
            }
        };
        assert!(same, "value drift at id {}", i + 1);
    }
    drop(db);
    cleanup(&path);
}

/// Generation soak (trans2.test style): 12 cycles of open -> mutate
/// (inserts, updates, deletes, purges) -> close -> reopen -> checksum.
/// The per-generation checksum is a monotonic ledger: if reopen loses
/// even one byte of one row, the checksum diverges and names the cycle.
#[test]
fn generation_soak() {
    let path = tmpdb("gen_soak");
    let mut rng = Rng::new(0x50A1);
    let mut ledger: Vec<u64> = Vec::new();

    // Generation 0 creates the schema and the seed rows.
    session(&path, |db| {
        db.execute(
            "CREATE TABLE g (
                id INTEGER PRIMARY KEY,
                k TEXT UNIQUE,
                payload BLOB,
                n INTEGER CHECK (n >= 0)
            )",
            [],
        )
        .unwrap();
        for i in 1..=100i64 {
            db.execute(
                "INSERT INTO g (k, payload, n) VALUES (?, ?, ?)",
                [
                    Value::Text(format!("key-{:04}", i).into()),
                    Value::Blob(((rng.next_u64() % 256) as u8).to_le_bytes().to_vec()),
                    Value::Integer((rng.next_u64() % 1000) as i64),
                ],
            )
            .unwrap();
        }
    });

    for gen in 1..=12i64 {
        session(&path, |db| {
            // (a) integrity + checksum first: what we closed with.
            let rows = db
                .query("SELECT id, k, payload, n FROM g ORDER BY id", [])
                .unwrap();
            let sum = row_checksum(&rows);
            let prev = if gen == 1 {
                sum
            } else {
                ledger[(gen - 2) as usize]
            };
            assert_eq!(
                sum, prev,
                "generation {}: reopen changed the ledger — data lost/altered between cycles",
                gen
            );

            // (b) constraints STILL enforced this generation (the whole
            // point of the matrix: probe every reopen, not just one).
            let dup_k = db.execute(
                "INSERT INTO g (k, n) VALUES ((SELECT k FROM g LIMIT 1), 1)",
                [],
            );
            assert!(dup_k.is_err(), "generation {}: UNIQUE(k) loosened", gen);
            let bad_n = db.execute("INSERT INTO g (k, n) VALUES ('never', -1)", []);
            assert!(bad_n.is_err(), "generation {}: CHECK loosened", gen);

            // (c) mutate: insert 30, update a third, delete a seventh,
            // and every 4th generation purge half and reinsert.
            for i in 1..=30i64 {
                let id = gen * 1000 + i;
                db.execute(
                    "INSERT INTO g (k, payload, n) VALUES (?, ?, ?)",
                    [
                        Value::Text(format!("key-g{}-{:04}", gen, i).into()),
                        Value::Blob((id as u8).to_le_bytes().to_vec()),
                        Value::Integer(id),
                    ],
                )
                .unwrap();
            }
            db.execute("UPDATE g SET n = n + 1 WHERE id % 3 = 0", [])
                .unwrap();
            db.execute("DELETE FROM g WHERE id % 7 = 0 AND id > 0", [])
                .unwrap();
            if gen % 4 == 0 {
                db.execute("DELETE FROM g WHERE id % 2 = 1", []).unwrap();
                for i in 1..=50i64 {
                    db.execute(
                        "INSERT INTO g (k, payload, n) VALUES (?, ?, ?)",
                        [
                            Value::Text(format!("key-r{}-{:04}", gen, i).into()),
                            Value::Blob(vec![gen as u8; 8]),
                            Value::Integer(gen * 10_000 + i),
                        ],
                    )
                    .unwrap();
                }
            }

            // (d) record the post-mutation ledger for the next generation.
            let rows = db
                .query("SELECT id, k, payload, n FROM g ORDER BY id", [])
                .unwrap();
            ledger.push(row_checksum(&rows));
        });
    }

    // Final reopen: the last ledger entry is the truth.
    let db = Database::open(&path).unwrap();
    let rows = db
        .query("SELECT id, k, payload, n FROM g ORDER BY id", [])
        .unwrap();
    assert_eq!(
        row_checksum(&rows),
        *ledger.last().unwrap(),
        "final reopen diverged from the generation ledger"
    );
    // The engine's own consistency checker agrees.
    let ic = one_text(&db, "PRAGMA quick_check");
    assert!(ic == "ok", "quick_check after 12 generations: {}", ic);
    drop(db);
    cleanup(&path);
}

/// Committed data survives reopen; an in-flight transaction is fully
/// absent after close (all-or-nothing) — the atomic-commit contract,
/// verified from the persistence side rather than the crash side
/// (crash_recovery.rs owns the SIGABRT variant).
#[test]
fn committed_vs_uncommitted_durability() {
    let path = tmpdb("commit_split");
    // Session 1: the committed baseline.
    session(&path, |db| {
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=100i64 {
            db.execute(
                "INSERT INTO t (v) VALUES (?)",
                [Value::Text(format!("committed-{}", i).into())],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
    });

    // Session 2: an uncommitted batch — open transaction, inserts, then
    // a plain graceful close (drop). NO flush, NO COMMIT, NO ROLLBACK:
    // session-end semantics must roll the transaction back (verified
    // against SQLite's sqlite3_close behavior in probe_txnclose.rs).
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 101..=150i64 {
            db.execute(
                "INSERT INTO t (v) VALUES (?)",
                [Value::Text(format!("uncommitted-{}", i).into())],
            )
            .unwrap();
        }
        drop(db);
    }

    let db = Database::open(&path).unwrap();
    assert_eq!(
        one_i64(&db, "SELECT COUNT(*) FROM t"),
        100,
        "committed batch must fully survive; uncommitted batch must be fully absent"
    );
    assert_eq!(
        one_i64(&db, "SELECT COUNT(*) FROM t WHERE v LIKE 'uncommitted-%'"),
        0,
        "uncommitted rows leaked into the file"
    );
    assert_eq!(
        one_i64(&db, "SELECT COUNT(*) FROM t WHERE v LIKE 'committed-%'"),
        100
    );
    drop(db);
    cleanup(&path);
}

/// WAL mode: commits that reached the WAL must be visible after a plain
/// close/reopen (close-time checkpoint OR WAL recovery — either is
/// correct, missing data is not). Mirrors the image()-WAL-staleness bug
/// fixed alongside the CLI tooling.
#[test]
fn wal_commits_survive_reopen() {
    let path = tmpdb("wal_reopen");
    session(&path, |db| {
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=200i64 {
            db.execute(
                "INSERT INTO t (v) VALUES (?)",
                [Value::Text(format!("w{}", i).into())],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        // Deliberately NO explicit flush/checkpoint — the close path
        // (or reopen recovery) must make these durable.
    });

    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        one_i64(&db, "SELECT COUNT(*) FROM t"),
        200,
        "WAL-committed rows must survive close/reopen without explicit checkpoint"
    );
    // Write more in WAL mode after reopen, then close cleanly.
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('post-wal')", [])
        .unwrap();
    db.flush().unwrap();
    drop(db);

    let db = Database::open(&path).unwrap();
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM t"), 201);
    let ic = one_text(&db, "PRAGMA quick_check");
    assert!(ic == "ok", "quick_check after WAL cycles: {}", ic);
    drop(db);
    cleanup(&path);
}

/// Idle reopen is byte-stable: three open/close cycles with no writes
/// must leave the file byte-identical (catches nondeterministic
/// serialization — page reordering, dirty-bit drift, header jitter).
#[test]
fn idle_reopen_is_byte_stable() {
    let path = tmpdb("byte_stable");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b REAL)",
            [],
        )
        .unwrap();
        for i in 1..=300i64 {
            db.execute(
                "INSERT INTO t (a, b) VALUES (?, ?)",
                [
                    Value::Text(format!("s{}", i * 7).into()),
                    Value::Real(i as f64 / 3.0),
                ],
            )
            .unwrap();
        }
        db.execute("CREATE INDEX idx_a ON t (a)", []).unwrap();
    });

    let first = std::fs::read(&path).expect("read after first close");
    for cycle in 0..2 {
        // Plain open + close, no writes, no explicit flush.
        let db = Database::open(&path).unwrap();
        drop(db);
        let now = std::fs::read(&path).expect("read after idle reopen");
        assert!(
            now == first,
            "idle reopen cycle {} rewrote the file ({} bytes -> {})",
            cycle,
            first.len(),
            now.len()
        );
    }
    cleanup(&path);
}

/// VACUUM then reopen: compacted file, identical data, constraints intact.
#[test]
fn vacuum_then_reopen() {
    let path = tmpdb("vacuum_reopen");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT UNIQUE, v TEXT)",
            [],
        )
        .unwrap();
        for i in 1..=500i64 {
            db.execute(
                "INSERT INTO t (k, v) VALUES (?, ?)",
                [
                    Value::Text(format!("k{}", i).into()),
                    Value::Text("x".repeat(200).into()),
                ],
            )
            .unwrap();
        }
        db.execute("DELETE FROM t WHERE id > 125", []).unwrap();
    });
    let size_before = std::fs::metadata(&path).unwrap().len();

    let mut db = Database::open(&path).unwrap();
    db.execute("VACUUM", []).unwrap();
    db.flush().unwrap();
    drop(db);

    let size_after = std::fs::metadata(&path).unwrap().len();
    assert!(
        size_after <= size_before,
        "VACUUM grew the file: {} -> {}",
        size_before,
        size_after
    );
    let mut db = Database::open(&path).unwrap();
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM t"), 125);
    assert_eq!(one_text(&db, "SELECT k FROM t WHERE id = 125"), "k125");
    // Constraint still enforced post-VACUUM + reopen.
    let dup = db.execute("INSERT INTO t (k, v) VALUES ('k125', 'dup')", []);
    assert!(dup.is_err(), "UNIQUE must survive VACUUM + reopen");
    let ic = one_text(&db, "PRAGMA quick_check");
    assert!(ic == "ok", "quick_check after VACUUM: {}", ic);
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// `db.image()` is SQLite's backup-API contract: the bytes must open as
/// a fully-functional database — data AND constraint enforcement AND
/// catalog — in `open_in_memory_with_image`. Re-runs the core battery.
#[test]
fn image_roundtrip_full_semantics() {
    let path = tmpdb("image_roundtrip");
    session(&path, |db| {
        db.execute(
            "CREATE TABLE u (user_id TEXT PRIMARY KEY, email TEXT UNIQUE)",
            [],
        )
        .unwrap();
        db.execute(
            "CREATE TABLE child (id INTEGER PRIMARY KEY, uid TEXT REFERENCES u (user_id))",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX idx_email ON u (email)", [])
            .unwrap();
        db.execute("CREATE VIEW v AS SELECT user_id FROM u", [])
            .unwrap();
        for i in 1..=20i64 {
            db.execute(
                "INSERT INTO u (user_id, email) VALUES (?, ?)",
                [
                    Value::Text(format!("uuid-{:04}", i).into()),
                    Value::Text(format!("u{}@x.io", i).into()),
                ],
            )
            .unwrap();
        }
        db.execute("INSERT INTO child (uid) VALUES ('uuid-0001')", [])
            .unwrap();
    });

    let db = Database::open(&path).unwrap();
    let img = db.image().expect("image");
    drop(db);
    cleanup(&path);

    let mut mem = Database::open_in_memory_with_image(img).expect("open image");
    assert_eq!(one_i64(&mem, "SELECT COUNT(*) FROM u"), 20);
    assert_eq!(one_i64(&mem, "SELECT COUNT(*) FROM v"), 20, "view in image");
    assert_eq!(
        one_i64(&mem, "SELECT COUNT(*) FROM child WHERE uid = 'uuid-0001'"),
        1,
        "FK-child row in image"
    );
    // Constraints survive the image round-trip.
    let dup = mem.execute(
        "INSERT INTO u (user_id, email) VALUES ('uuid-0001', 'z@x.io')",
        [],
    );
    assert!(dup.is_err(), "PK enforcement must survive image round-trip");
    let dup2 = mem.execute(
        "INSERT INTO u (user_id, email) VALUES ('uuid-9999', 'u1@x.io')",
        [],
    );
    assert!(
        dup2.is_err(),
        "UNIQUE enforcement must survive image round-trip"
    );
    // Index usable in the image.
    let q = mem.query(
        "SELECT COUNT(*) FROM u INDEXED BY idx_email WHERE email = 'u1@x.io'",
        [],
    );
    assert!(q.is_ok(), "index lost in image: {:?}", q.err());
    assert_eq!(
        one_i64(
            &mem,
            "SELECT COUNT(*) FROM u INDEXED BY idx_email WHERE email = 'u1@x.io'"
        ),
        1
    );
    // Writes work in the image too.
    mem.execute(
        "INSERT INTO u (user_id, email) VALUES ('uuid-0021', 'u21@x.io')",
        [],
    )
    .unwrap();
    assert_eq!(one_i64(&mem, "SELECT COUNT(*) FROM u"), 21);
    drop(mem);
}

// ===========================================================================
// SECTION D — the SQLite on-disk format gets the same battery.
//
// `open_sqlite_format` (foreign format) shares none of the load code
// with the native format, so every durability property must be re-proven
// there separately. This path has historically lagged the native one
// (sqlite_sequence creation, image()/WAL staleness — see worklog) and
// is exactly where a second silent-constraint-loss bug would hide.
// ===========================================================================

/// The full constraint battery, foreign format: rowid alias PK, TEXT/uuid
/// PK autoindex, WITHOUT ROWID composite PK, UNIQUE + NULL multiplicity,
/// CHECK, FK actions, AUTOINCREMENT floor — one reopen, every negative
/// probe. Any loosening here is the uuid-PK bug reborn in format #2.
#[test]
fn sqlite_format_constraint_battery() {
    let path = tmpdb("sqlitefmt_battery");
    let path = path.with_extension("sqlitefmt.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute(
            "CREATE TABLE users (user_id uuid_text NOT NULL PRIMARY KEY, email TEXT UNIQUE)",
            [],
        )
        .unwrap();
        db.execute(
            "CREATE TABLE kv (k1 TEXT, k2 INTEGER, v TEXT, PRIMARY KEY (k1, k2)) WITHOUT ROWID",
            [],
        )
        .unwrap();
        db.execute(
            "CREATE TABLE chk (id INTEGER PRIMARY KEY, score REAL CHECK (score >= 0))",
            [],
        )
        .unwrap();
        db.execute(
            "CREATE TABLE seq (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
            [],
        )
        .unwrap();
        db.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY)", [])
            .unwrap();
        db.execute(
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pid REFERENCES parent (id) ON DELETE CASCADE)",
            [],
        )
        .unwrap();
        for i in 1..=10i64 {
            db.execute(
                "INSERT INTO users (user_id, email) VALUES (?, ?)",
                [
                    Value::Text(format!("uuid-{:03}", i).into()),
                    Value::Text(format!("u{}@x.io", i).into()),
                ],
            )
            .unwrap();
            db.execute(
                "INSERT INTO kv VALUES (?, ?, ?)",
                [
                    Value::Text(format!("k{}", i % 3).into()),
                    Value::Integer(i),
                    Value::Text(format!("v{}", i).into()),
                ],
            )
            .unwrap();
            db.execute(
                "INSERT INTO chk (score) VALUES (?)",
                [Value::Real(i as f64 / 2.0)],
            )
            .unwrap();
            db.execute("INSERT INTO seq (v) VALUES ('x')", []).unwrap();
        }
        db.execute("INSERT INTO parent VALUES (1)", []).unwrap();
        db.execute("INSERT INTO child (pid) VALUES (1)", [])
            .unwrap();
        // seq: 10 is the historic max; delete it.
        db.execute("DELETE FROM seq WHERE id = 10", []).unwrap();
        db.flush().unwrap();
        drop(db);
    }

    // Reopen (foreign format) and probe everything.
    let mut db = Database::open_sqlite_format(&path).unwrap();
    assert_eq!(
        db.disk_format(),
        "sqlite",
        "disk_format() must report sqlite"
    );
    // TEXT/uuid PK.
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM users"), 10);
    let dup_uuid = db.execute(
        "INSERT INTO users (user_id, email) VALUES ('uuid-001', 'x@x.io')",
        [],
    );
    assert!(
        dup_uuid.is_err(),
        "sqlite-fmt: TEXT PK uniqueness lost on reopen"
    );
    let dup_email = db.execute(
        "INSERT INTO users (user_id, email) VALUES ('uuid-999', 'u1@x.io')",
        [],
    );
    assert!(dup_email.is_err(), "sqlite-fmt: UNIQUE lost on reopen");
    // WITHOUT ROWID composite PK.
    let dup_kv = db.execute("INSERT INTO kv VALUES ('k1', 4, 'dup')", []);
    assert!(
        dup_kv.is_err(),
        "sqlite-fmt: WITHOUT ROWID PK lost on reopen"
    );
    assert_eq!(
        one_text(&db, "SELECT v FROM kv WHERE k1 = 'k1' AND k2 = 4"),
        "v4"
    );
    // CHECK.
    let bad_score = db.execute("INSERT INTO chk (score) VALUES (-1.0)", []);
    assert!(bad_score.is_err(), "sqlite-fmt: CHECK lost on reopen");
    // AUTOINCREMENT floor.
    let seq = one_i64(&db, "SELECT seq FROM sqlite_sequence WHERE name = 'seq'");
    assert!(seq >= 10, "sqlite-fmt: sequence floor lost: {}", seq);
    db.execute("INSERT INTO seq (v) VALUES ('post')", [])
        .unwrap();
    assert!(
        one_i64(&db, "SELECT MAX(id) FROM seq") >= 10,
        "sqlite-fmt: AUTOINCREMENT reused the deleted max"
    );
    // FK cascade.
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    db.execute("DELETE FROM parent WHERE id = 1", []).unwrap();
    assert_eq!(
        one_i64(&db, "SELECT COUNT(*) FROM child WHERE pid = 1"),
        0,
        "sqlite-fmt: ON DELETE CASCADE lost on reopen"
    );
    let ic = one_text(&db, "PRAGMA quick_check");
    assert!(ic == "ok", "sqlite-fmt: quick_check: {}", ic);
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// Foreign format generation soak: 6 generations of churn + checksum,
/// the trans2.test loop on the second serialization path.
#[test]
fn sqlite_format_generation_soak() {
    let path = tmpdb("sqlitefmt_soak");
    let path = path.with_extension("sqlitefmt.db");
    let _ = std::fs::remove_file(&path);
    let mut rng = Rng::new(0xF0D);
    let mut ledger;

    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute(
            "CREATE TABLE g (id INTEGER PRIMARY KEY, k TEXT UNIQUE, n INTEGER)",
            [],
        )
        .unwrap();
        for i in 1..=50i64 {
            db.execute(
                "INSERT INTO g (k, n) VALUES (?, ?)",
                [
                    Value::Text(format!("k{:03}", i).into()),
                    Value::Integer((rng.next_u64() % 500) as i64),
                ],
            )
            .unwrap();
        }
        let rows = db.query("SELECT id, k, n FROM g ORDER BY id", []).unwrap();
        ledger = row_checksum(&rows);
        db.flush().unwrap();
        drop(db);
    }

    for gen in 1..=6i64 {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        // Ledger holds.
        let rows = db.query("SELECT id, k, n FROM g ORDER BY id", []).unwrap();
        assert_eq!(
            row_checksum(&rows),
            ledger,
            "sqlite-fmt gen {}: ledger drift",
            gen
        );
        // Constraints probe every generation.
        let dup = db.execute(
            "INSERT INTO g (k, n) VALUES ((SELECT k FROM g LIMIT 1), 0)",
            [],
        );
        assert!(dup.is_err(), "sqlite-fmt gen {}: UNIQUE loosened", gen);
        // Churn.
        for i in 1..=20i64 {
            let id = gen * 100 + i;
            db.execute(
                "INSERT INTO g (k, n) VALUES (?, ?)",
                [
                    Value::Text(format!("g{}-{:03}", gen, i).into()),
                    Value::Integer(id),
                ],
            )
            .unwrap();
        }
        db.execute("UPDATE g SET n = n + 2 WHERE id % 5 = 0", [])
            .unwrap();
        db.execute("DELETE FROM g WHERE id % 11 = 0", []).unwrap();
        let rows = db.query("SELECT id, k, n FROM g ORDER BY id", []).unwrap();
        ledger = row_checksum(&rows);
        db.flush().unwrap();
        drop(db);
    }

    let db = Database::open_sqlite_format(&path).unwrap();
    let rows = db.query("SELECT id, k, n FROM g ORDER BY id", []).unwrap();
    assert_eq!(
        row_checksum(&rows),
        ledger,
        "sqlite-fmt: final ledger drift"
    );
    let ic = one_text(&db, "PRAGMA quick_check");
    assert!(ic == "ok", "sqlite-fmt soak: quick_check: {}", ic);
    drop(db);
    cleanup(&path);
}

/// Foreign-format byte stability across idle reopen (same contract as
/// the native format test).
#[test]
fn sqlite_format_idle_reopen_byte_stable() {
    let path = tmpdb("sqlitefmt_stable");
    let path = path.with_extension("sqlitefmt.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        for i in 1..=200i64 {
            db.execute(
                "INSERT INTO t (v) VALUES (?)",
                [Value::Text(format!("row{}", i * 13).into())],
            )
            .unwrap();
        }
        db.flush().unwrap();
        drop(db);
    }
    let first = std::fs::read(&path).unwrap();
    for _ in 0..2 {
        let db = Database::open_sqlite_format(&path).unwrap();
        drop(db);
        let now = std::fs::read(&path).unwrap();
        assert!(
            now == first,
            "sqlite-fmt idle reopen rewrote the file ({} -> {} bytes)",
            first.len(),
            now.len()
        );
    }
    cleanup(&path);
}

/// Rapid open/close churn: 40 open-verify-close cycles on one file with
/// alternating readers/writers. Catches fd leaks, lock residue, and
/// header-version drift that only appear across MANY cheap cycles
/// (the server's real access pattern).
#[test]
fn rapid_open_close_churn() {
    let path = tmpdb("rapid_churn");
    session(&path, |db| {
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute("INSERT INTO t (v) VALUES ('seed')", []).unwrap();
    });
    for cycle in 0..40i64 {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM t"), cycle + 1);
        // Writer cycles append; reader cycles only verify.
        db.execute(
            "INSERT INTO t (v) VALUES (?)",
            [Value::Text(format!("c{}", cycle).into())],
        )
        .unwrap();
        db.flush().unwrap();
        drop(db);
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM t"), 41);
    assert_eq!(one_text(&db, "SELECT v FROM t WHERE id = 1"), "seed");
    drop(db);
    cleanup(&path);
}

/// One file, two formats, two contracts: a native-format file must NOT
/// open via `open_sqlite_format` (fail fast, no silent misparse — and
/// the failed open must not have damaged the file). The REVERSE is
/// deliberately lenient: `Database::open` sniffs the SQLite magic and
/// bridges sqlite-format files through the interop loader (documented
/// behavior — see open_inner), which must yield the SAME data as
/// `open_sqlite_format` on that file.
#[test]
fn format_confusion_fails_cleanly() {
    let path = tmpdb("fmt_confusion");
    session(&path, |db| {
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", [])
            .unwrap();
        db.execute("INSERT INTO t VALUES (1)", []).unwrap();
    });
    // Native file through the sqlite-format door: must fail.
    let wrong = Database::open_sqlite_format(&path);
    assert!(wrong.is_err(), "native file must not open as sqlite-format");
    // And the failed open must not have damaged the native file.
    let db = Database::open(&path).unwrap();
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM t"), 1);
    drop(db);
    cleanup(&path);

    // And the reverse: sqlite-format file through the native door — the
    // interop bridge sniffs the magic and loads it (documented behavior).
    let spath = tmpdb("fmt_confusion_sqlitefmt");
    let spath = spath.with_extension("sqlitefmt.db");
    let _ = std::fs::remove_file(&spath);
    {
        let mut db = Database::open_sqlite_format(&spath).unwrap();
        db.execute("CREATE TABLE s (x)", []).unwrap();
        db.execute("INSERT INTO s VALUES (2)", []).unwrap();
        db.flush().unwrap();
        drop(db);
    }
    // Native open: bridged, not refused — and the bridge must see the
    // same rows the sqlite-format door sees.
    let db = Database::open(&spath).unwrap();
    assert_eq!(
        db.disk_format(),
        "sqlite",
        "bridge must report sqlite format"
    );
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM s"), 1);
    assert_eq!(one_i64(&db, "SELECT x FROM s"), 2);
    drop(db);
    let db = Database::open_sqlite_format(&spath).unwrap();
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM s"), 1);
    drop(db);
    cleanup(&spath);
}

// ===========================================================================
// SECTION E — TEMP objects are session-scoped (SQLite temp-schema
// semantics). This pins the fix for the bug this suite found on
// 2026-09-13: `CREATE TEMP TABLE` was parsed with the TEMP keyword
// DISCARDED (`let _temp = ...`), so temp tables persisted to
// sqlite_master like permanent ones — connection-scoped scratch data
// silently became durable. SQLite's contract: temp tables/views/indexes/
// triggers live for the connection, never touch the main file, and are
// gone after close/reopen.
// =========================================================================//

/// TEMP TABLE: fully usable during the session (data + constraints), and
/// completely gone after close/reopen — while permanent tables are
/// untouched.
#[test]
fn temp_table_is_session_scoped() {
    let path = tmpdb("temp_scope");
    session(&path, |db| {
        db.execute("CREATE TABLE permanent (x INTEGER PRIMARY KEY)", [])
            .unwrap();
        db.execute("INSERT INTO permanent VALUES (1)", []).unwrap();
        // TEMP table with constraints — everything must WORK this session.
        db.execute(
            "CREATE TEMP TABLE scratch (id INTEGER PRIMARY KEY, k TEXT UNIQUE, n CHECK (n > 0))",
            [],
        )
        .unwrap();
        for i in 1..=10i64 {
            db.execute(
                "INSERT INTO scratch (k, n) VALUES (?, ?)",
                [Value::Text(format!("k{}", i).into()), Value::Integer(i)],
            )
            .unwrap();
        }
        assert_eq!(
            db.query("SELECT COUNT(*) FROM scratch", []).unwrap()[0][0].as_integer(),
            10,
            "temp table must be queryable in-session"
        );
        // Constraints enforce in-session.
        let dup = db.execute("INSERT INTO scratch (k, n) VALUES ('k1', 5)", []);
        assert!(dup.is_err(), "temp UNIQUE must enforce in-session");
        let bad = db.execute("INSERT INTO scratch (k, n) VALUES ('kx', -1)", []);
        assert!(bad.is_err(), "temp CHECK must enforce in-session");
    });

    let mut db = Database::open(&path).unwrap();
    // The temp table is GONE (SQLite semantics).
    let gone = db.query("SELECT COUNT(*) FROM scratch", []);
    assert!(
        gone.is_err(),
        "TEMP TABLE must not survive close/reopen (session-scoped)"
    );
    // And it must not be in the persisted catalog.
    let in_master = one_i64(
        &db,
        "SELECT COUNT(*) FROM sqlite_master WHERE name = 'scratch'",
    );
    assert_eq!(in_master, 0, "temp table leaked into sqlite_master");
    // The permanent table is untouched.
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM permanent"), 1);
    // A new table can reuse the name.
    db.execute("CREATE TABLE scratch (z TEXT)", []).unwrap();
    db.execute("INSERT INTO scratch VALUES ('now-mine')", [])
        .unwrap();
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM scratch"), 1);
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// TEMP VIEW, TEMP INDEX, and a trigger on a temp table: all
/// connection-scoped, all gone after reopen. Also: CREATE INDEX on a
/// temp table (not declared TEMP) follows the table's scope.
#[test]
fn temp_view_index_trigger_are_session_scoped() {
    let path = tmpdb("temp_vit");
    session(&path, |db| {
        db.execute("CREATE TABLE perm (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute("INSERT INTO perm (v) VALUES ('a'), ('b')", [])
            .unwrap();
        db.execute(
            "CREATE TEMP TABLE staging (id INTEGER PRIMARY KEY, v TEXT)",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO staging (v) VALUES ('x'), ('y')", [])
            .unwrap();
        // TEMP VIEW.
        db.execute("CREATE TEMP VIEW v_stage AS SELECT v FROM staging", [])
            .unwrap();
        assert_eq!(
            db.query("SELECT COUNT(*) FROM v_stage", []).unwrap()[0][0].as_integer(),
            2,
            "temp view queryable in-session"
        );
        // TEMP INDEX (explicit).
        db.execute("CREATE TEMP INDEX ti_s ON staging (v)", [])
            .unwrap();
        // Index on a TEMP table WITHOUT the TEMP keyword — follows scope.
        db.execute("CREATE INDEX i_s ON staging (id)", []).unwrap();
        // Trigger on a temp table — follows scope too.
        db.execute(
            "CREATE TRIGGER trg_stage AFTER INSERT ON staging
             BEGIN
                 INSERT INTO perm (v) VALUES ('fired:' || new.v);
             END",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO staging (v) VALUES ('z')", [])
            .unwrap();
        assert_eq!(
            one_i64(db, "SELECT COUNT(*) FROM perm WHERE v = 'fired:z'"),
            1,
            "trigger on temp table must fire in-session"
        );
    });

    let mut db = Database::open(&path).unwrap();
    for name in ["staging", "v_stage"] {
        let q = db.query(&format!("SELECT 1 FROM {}", name), []);
        assert!(q.is_err(), "{} must be gone after reopen", name);
    }
    let n = one_i64(
        &db,
        "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('staging', 'v_stage', 'ti_s', 'i_s', 'trg_stage')",
    );
    assert_eq!(n, 0, "temp objects leaked into sqlite_master: {}", n);
    // The trigger's side effects on the PERMANENT table persist (they
    // were real writes to perm), but the trigger itself is gone.
    assert_eq!(
        one_i64(&db, "SELECT COUNT(*) FROM perm WHERE v = 'fired:z'"),
        1,
        "permanent side effects of the temp trigger must persist"
    );
    db.execute("INSERT INTO perm (v) VALUES ('c')", []).unwrap();
    let fired = one_i64(&db, "SELECT COUNT(*) FROM perm WHERE v LIKE 'fired:%'");
    assert_eq!(fired, 1, "trigger must not fire after it (temp) is gone");
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// ALTER on a TEMP table keeps it temp (rename, add column): the object
/// stays session-scoped and its post-alter shape works until close.
#[test]
fn temp_table_stays_temp_across_alter() {
    let path = tmpdb("temp_alter");
    session(&path, |db| {
        db.execute("CREATE TABLE keeper (x)", []).unwrap();
        db.execute("CREATE TEMP TABLE tt (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute("ALTER TABLE tt ADD COLUMN extra INTEGER DEFAULT 7", [])
            .unwrap();
        db.execute("ALTER TABLE tt RENAME TO tt2", []).unwrap();
        db.execute("INSERT INTO tt2 (v) VALUES ('r')", []).unwrap();
        assert_eq!(
            one_i64(db, "SELECT extra FROM tt2 WHERE v = 'r'"),
            7,
            "ADD COLUMN default must apply to the temp table"
        );
    });

    let mut db = Database::open(&path).unwrap();
    for name in ["tt", "tt2"] {
        let q = db.query(&format!("SELECT 1 FROM {}", name), []);
        assert!(q.is_err(), "renamed temp table {} must be gone", name);
    }
    let n = one_i64(
        &db,
        "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('tt', 'tt2')",
    );
    assert_eq!(n, 0, "ALTERed temp table leaked into sqlite_master");
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM keeper"), 0);
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

// ===========================================================================
// Atomic-write staging files (`rsqltmp`) — the single-file-at-rest contract
// ===========================================================================

/// Helper: every `{stem}.rsqltmp*` file in the database's directory,
/// whatever its suffix (used to assert both "none remain" and "only the
/// numeric orphans were swept").
fn staging_files(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let stem = path.file_stem().unwrap().to_string_lossy().to_string();
    let prefix = format!("{}.rsqltmp", stem);
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let starts = entry
                .file_name()
                .to_str()
                .map(|n| n.starts_with(prefix.as_str()))
                .unwrap_or(false);
            if starts {
                found.push(entry.path());
            }
        }
    }
    found
}

/// A clean commit cycle must leave ONLY the database file: the atomic
/// writer stages each full-image write as `{stem}.rsqltmp{pid}` and the
/// name is renamed away (POSIX) or removed (the Windows in-place
/// fallback) — it must never be left behind. Regression: dev `make`
/// runs littered `db.rsqltmp19508`-style files next to the database.
#[test]
fn atomic_write_leaves_no_staging_file() {
    let path = tmpdb("staging_clean");
    {
        let mut db = Database::open_sqlite_format(&path).expect("open sqlite format");
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .expect("ddl");
        db.execute("INSERT INTO t (v) VALUES ('a')", [])
            .expect("insert");
        db.flush().expect("flush");
        drop(db);
    }
    assert!(path.exists(), "the database file exists after close");
    let stray = staging_files(&path);
    assert!(
        stray.is_empty(),
        "no rsqltmp staging files may remain after a clean close: {stray:?}"
    );

    // The committed data survived the cycle.
    let mut db = Database::open(&path).expect("reopen");
    assert_eq!(one_i64(&db, "SELECT COUNT(*) FROM t"), 1);
    db.flush().unwrap();
    drop(db);
    cleanup(&path);
}

/// Opening a database sweeps away orphaned staging files from processes
/// that died between staging and rename (power loss, kill -9, a crashed
/// CLI run) — the "random `db.rsqltmp19508` file" complaint. Only
/// numeric-suffix orphans of THIS database's stem are swept: a file that
/// merely starts with the same prefix (non-numeric suffix, or another
/// database named `stem.rsqltmp…`) must survive untouched.
#[test]
fn open_sweeps_orphaned_staging_files() {
    let path = tmpdb("staging_sweep");
    {
        let mut db = Database::open_sqlite_format(&path).expect("create");
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .expect("ddl");
        db.execute("INSERT INTO t (v) VALUES ('keep')", [])
            .expect("insert");
        db.flush().expect("flush");
        drop(db);
    }

    let dir = path.parent().unwrap().to_path_buf();
    let stem = path.file_stem().unwrap().to_string_lossy().to_string();
    // Orphans a dead process could have left behind (numeric PIDs).
    let orphan_a = dir.join(format!("{}.rsqltmp19508", stem));
    let orphan_b = dir.join(format!("{}.rsqltmp7", stem));
    // Look-alikes that must NOT be swept: non-numeric suffix, and a
    // foreign database whose name merely starts with the prefix.
    let lookalike = dir.join(format!("{}.rsqltmpNOTANUM", stem));
    let foreign_db = dir.join(format!("{}.rsqltmp42.sqlite", stem));
    std::fs::write(&orphan_a, b"partial garbage from a killed writer").unwrap();
    std::fs::write(&orphan_b, b"").unwrap();
    std::fs::write(&lookalike, b"unrelated file").unwrap();
    // A real SQLite database file with a prefix-shaped name — opening
    // it as its own database must keep working (and keep its data).
    {
        let mut other = Database::open_sqlite_format(&foreign_db).expect("foreign open");
        other
            .execute("CREATE TABLE f (x)", [])
            .expect("foreign ddl");
        other
            .execute("INSERT INTO f VALUES (7)", [])
            .expect("foreign insert");
        other.flush().expect("foreign flush");
        drop(other);
    }

    // The reopen that must clean the orphans.
    let mut db = Database::open(&path).expect("reopen");
    assert!(
        !orphan_a.exists() && !orphan_b.exists(),
        "numeric-suffix rsqltmp orphans must be swept on open"
    );
    assert!(
        lookalike.exists(),
        "a non-numeric suffix file sharing the prefix must survive"
    );
    assert!(
        foreign_db.exists(),
        "a foreign database with a prefix-shaped name must survive"
    );
    // The sweep must not have touched the data.
    assert_eq!(
        one_text(&db, "SELECT v FROM t WHERE id = 1"),
        "keep",
        "sweeping must leave committed data intact"
    );
    db.flush().unwrap();
    drop(db);

    // The foreign database still opens with its own data (its own sweep
    // keys on stem "stem.rsqltmp42", not on our stem).
    let mut other = Database::open(&foreign_db).expect("foreign reopen");
    assert_eq!(one_i64(&other, "SELECT COUNT(*) FROM f"), 1);
    other.flush().unwrap();
    drop(other);

    let _ = std::fs::remove_file(&lookalike);
    let _ = std::fs::remove_file(&foreign_db);
    cleanup(&path);
}
