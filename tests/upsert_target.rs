//! UPSERT conflict-target resolution follows sqlite3UpsertAnalyzeTarget:
//! a single-term target naming the rowid (its INTEGER PRIMARY KEY alias,
//! or `rowid` / `oid` / `_rowid_`) is the rowid — checked before every
//! index, even a UNIQUE index on the same column (stateful-fuzz seed
//! 64064: `ON CONFLICT(id) DO NOTHING` raised the `note` index's UNIQUE
//! error instead of doing nothing for an existing id); otherwise the
//! first UNIQUE index whose key columns the target lists, where a PARTIAL
//! index needs the target to repeat its WHERE and a target COLLATE must
//! name the index column's collation. Every statement's success / error
//! and the final table contents must equal the bundled SQLite's.

use rusqlite::types::Value as Sv;
use rusqlite::Connection;
use rustqlite::{Database, Value};

fn ours_rows(db: &mut Database, sql: &str) -> Vec<String> {
    db.query(sql, [])
        .unwrap()
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::Integer(i) => format!("I{i}"),
                    Value::Real(f) => format!("R{f:?}"),
                    Value::Text(t) => format!("T{}", t.as_str()),
                    Value::Blob(b) => format!("B{b:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn sqlite_rows(c: &Connection, sql: &str) -> Vec<String> {
    let mut st = c.prepare(sql).unwrap();
    let n = st.column_count();
    let mut rows = st.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        out.push(
            (0..n)
                .map(|i| match r.get::<_, Sv>(i).unwrap() {
                    Sv::Null => "NULL".to_string(),
                    Sv::Integer(i) => format!("I{i}"),
                    Sv::Real(f) => format!("R{f:?}"),
                    Sv::Text(t) => format!("T{t}"),
                    Sv::Blob(b) => format!("B{b:?}"),
                })
                .collect::<Vec<_>>()
                .join("|"),
        );
    }
    out
}

#[test]
fn upsert_target_resolution_matches_sqlite() {
    const SETUP: &[&str] = &[
        "CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT)",
        "CREATE UNIQUE INDEX ix_note ON audit(note) WHERE note IS NOT NULL",
        "CREATE UNIQUE INDEX ix_id ON audit(id) WHERE id IS NOT NULL",
        "INSERT INTO audit VALUES (7, 'a'), (8, 'b')",
        "CREATE TABLE w (a TEXT PRIMARY KEY, b) WITHOUT ROWID",
        "INSERT INTO w VALUES ('x', 1)",
        "CREATE TABLE ex (a, b, c TEXT COLLATE NOCASE)",
        "CREATE UNIQUE INDEX ex_ab ON ex(lower(a), b)",
        "CREATE UNIQUE INDEX ex_c ON ex(c)",
        "INSERT INTO ex VALUES ('A', 1, 'k')",
        "CREATE TABLE m (p, q, r, UNIQUE (p, q))",
        "INSERT INTO m VALUES (1, 2, 'r')",
    ];
    const STATEMENTS: &[&str] = &[
        // The rowid target wins over the index on the same column and
        // is checked before the `note` index (seed 64064).
        "INSERT INTO audit (id, note) VALUES (7, 'b') ON CONFLICT(id) DO NOTHING",
        "INSERT INTO audit (id, note) VALUES (7, 'b') ON CONFLICT(ID) DO UPDATE SET note = 'c'",
        "INSERT INTO audit (id, note) VALUES (7, 'q') ON CONFLICT(rowid) DO UPDATE SET note = 'r'",
        "INSERT INTO audit (id, note) VALUES (7, 'q') ON CONFLICT(oid) DO NOTHING",
        "INSERT INTO audit (id, note) VALUES (7, 'q') ON CONFLICT(_rowid_) DO NOTHING",
        // A partial index needs the target's WHERE.
        "INSERT INTO audit (id, note) VALUES (9, 'a') ON CONFLICT(note) DO NOTHING",
        "INSERT INTO audit (id, note) VALUES (9, 'b') ON CONFLICT(note) WHERE note IS NOT NULL DO NOTHING",
        "INSERT INTO audit (id, note) VALUES (10, 'b') ON CONFLICT(note) WHERE note IS NOT NULL DO UPDATE SET id = 11",
        "INSERT INTO audit (id, note) VALUES (12, 'b') ON CONFLICT(note) WHERE note > '' DO NOTHING",
        "INSERT INTO audit (id, note) VALUES (12, 'b') ON CONFLICT(note COLLATE NOCASE) WHERE note IS NOT NULL DO NOTHING",
        "INSERT INTO audit (id, note) VALUES (12, 'b') ON CONFLICT(note COLLATE binary) WHERE note IS NOT NULL DO NOTHING",
        // No rowid on a WITHOUT ROWID table.
        "INSERT INTO w VALUES ('x', 2) ON CONFLICT(rowid) DO NOTHING",
        "INSERT INTO w VALUES ('x', 2) ON CONFLICT(a) DO UPDATE SET b = 3",
        // Expression keys, any term order; collation must match exactly.
        "INSERT INTO ex VALUES ('a', 1, 'z') ON CONFLICT(b, lower(a)) DO UPDATE SET b = 5",
        "INSERT INTO ex VALUES ('a', 3, 'z') ON CONFLICT(lower(a)) DO NOTHING",
        "INSERT INTO ex VALUES ('q', 9, 'K') ON CONFLICT(c) DO UPDATE SET b = 6",
        "INSERT INTO ex VALUES ('q', 9, 'K') ON CONFLICT(c COLLATE nocase) DO UPDATE SET b = 7",
        "INSERT INTO ex VALUES ('q', 9, 'K') ON CONFLICT(c COLLATE BINARY) DO NOTHING",
        // Multi-column UNIQUE: any term order, exact column set.
        "INSERT INTO m VALUES (1, 2, 's') ON CONFLICT(q, p) DO UPDATE SET r = 't'",
        "INSERT INTO m VALUES (1, 2, 's') ON CONFLICT(p) DO NOTHING",
        "INSERT INTO m VALUES (1, 2, 's') ON CONFLICT(p, q, r) DO NOTHING",
        "INSERT INTO m VALUES (1, 2, 's') ON CONFLICT DO NOTHING",
    ];
    let mut db = Database::open_in_memory().unwrap();
    let c = Connection::open_in_memory().unwrap();
    for s in SETUP {
        db.execute(s, []).unwrap();
        c.execute_batch(s).unwrap();
    }
    for s in STATEMENTS {
        let ours = db.execute(s, []);
        let theirs = c.execute_batch(s);
        assert_eq!(
            ours.is_ok(),
            theirs.is_ok(),
            "{s}: ours {ours:?} vs sqlite {theirs:?}"
        );
        for q in [
            "SELECT * FROM audit ORDER BY id",
            "SELECT * FROM w ORDER BY a",
            "SELECT a, b, c FROM ex ORDER BY rowid",
            "SELECT * FROM m ORDER BY rowid",
        ] {
            assert_eq!(ours_rows(&mut db, q), sqlite_rows(&c, q), "after {s}: {q}");
        }
    }
}

/// DO UPDATE is a full UPDATE: a SET on the INTEGER PRIMARY KEY moves the
/// row to the new rowid, with the UPDATE's rowid-uniqueness check (ABORT)
/// and OP_MustBeInt's "datatype mismatch" for NULL / non-integers. It used
/// to be silently pinned to the old rowid (`ON CONFLICT(note) DO UPDATE
/// SET id = 11` left the row at its old id).
#[test]
fn upsert_do_update_moves_the_rowid() {
    const SETUP: &[&str] = &[
        "CREATE TABLE a (id INTEGER PRIMARY KEY, note TEXT UNIQUE)",
        "CREATE INDEX a_note2 ON a(note, id)",
        "INSERT INTO a VALUES (7, 'a'), (8, 'b')",
        "CREATE TABLE log (e TEXT)",
        "CREATE TRIGGER a_bu BEFORE UPDATE ON a BEGIN INSERT INTO log VALUES ('bu ' || old.id || '>' || new.id); END",
        "CREATE TRIGGER a_au AFTER UPDATE ON a BEGIN INSERT INTO log VALUES ('au ' || old.id || '>' || new.id); END",
    ];
    const STATEMENTS: &[&str] = &[
        "INSERT INTO a (id, note) VALUES (10, 'b') ON CONFLICT(note) DO UPDATE SET id = 11",
        "INSERT INTO a (id, note) VALUES (10, 'b') ON CONFLICT(note) DO UPDATE SET id = 7",
        "INSERT INTO a (id, note) VALUES (10, 'b') ON CONFLICT(note) DO UPDATE SET id = NULL",
        "INSERT INTO a (id, note) VALUES (10, 'b') ON CONFLICT(note) DO UPDATE SET id = 'x'",
        "INSERT INTO a (id, note) VALUES (10, 'b') ON CONFLICT(note) DO UPDATE SET id = '14', note = 'c'",
        "INSERT INTO a (id, note) VALUES (10, 'c') ON CONFLICT(note) DO UPDATE SET id = excluded.id + 1",
        "INSERT INTO a (id, note) VALUES (7, 'z') ON CONFLICT(id) DO UPDATE SET id = 3",
        "INSERT INTO a (id, note) VALUES (5, 'q'), (11, 'a') ON CONFLICT(note) DO UPDATE SET id = 12",
    ];
    let mut db = Database::open_in_memory().unwrap();
    let c = Connection::open_in_memory().unwrap();
    for s in SETUP {
        db.execute(s, []).unwrap();
        c.execute_batch(s).unwrap();
    }
    for s in STATEMENTS {
        let ours = db.execute(s, []);
        let theirs = c.execute_batch(s);
        assert_eq!(
            ours.is_ok(),
            theirs.is_ok(),
            "{s}: ours {ours:?} vs sqlite {theirs:?}"
        );
        for q in [
            "SELECT * FROM a ORDER BY id",
            "SELECT note, id FROM a INDEXED BY a_note2 WHERE note > '' ORDER BY note",
            "SELECT * FROM log ORDER BY rowid",
            "PRAGMA integrity_check",
        ] {
            assert_eq!(ours_rows(&mut db, q), sqlite_rows(&c, q), "after {s}: {q}");
        }
    }
}
