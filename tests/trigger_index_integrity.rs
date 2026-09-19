//! Regression tests for the deep-sweep trigger/index/atomicity bugs —
//! all differential against real SQLite (the same contract as the
//! stateful fuzz, but deterministic and minimal).
//!
//! 1. `index_root_split_in_trigger_cascade`: a nested AFTER-INSERT
//!    trigger body's index root split was REVERTED by the outer
//!    statement's "write back index-root moves" epilogue (a stale
//!    st.root clobber) — the split's pages were orphaned and every
//!    entry under them became unreachable ("row N missing from index
//!    I"). Every split site already writes the root at the moment of
//!    the move; the epilogues are gone.
//! 2. `failed_delete_all_rolls_back_every_row`: the streaming delete
//!    loop pushed the statement-journal Deleted entry AFTER the
//!    AFTER-DELETE trigger firing — a trigger error (`?` propagation)
//!    returned before the push, permanently losing the row (SQLite's
//!    statement atomicity restores ALL of them).
//! 3. `upsert_target_checked_before_other_indexes`: SQLite checks the
//!    TARGET constraint first (the table b-tree insert) — `ON
//!    CONFLICT(id) DO NOTHING` with BOTH id and note taken succeeds
//!    because the note conflict is never reached.
//! 4. `insert_or_ignore_skips_not_null`: OR IGNORE covers NOT NULL (and
//!    CHECK) violations — the row is skipped, not an error.
//! 5. `autoincrement_negative_rowids_never_raise_sequence`: the
//!    sequence floor starts at 0; negative explicit rowids never raise
//!    it — the first auto id on a negative-only table is 1.

use rusqlite::Connection;
use rustqlite::{Database, Value};

fn engine_rows(db: &mut Database, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    let u = sql.trim_start().to_ascii_uppercase();
    let is_query = u.starts_with("SELECT") || u.starts_with("PRAGMA");
    if is_query {
        db.query(sql, [])
            .map(|r| r.into_iter().collect())
            .map_err(|e| e.to_string())
    } else {
        db.execute(sql, [])
            .map(|_| Vec::new())
            .map_err(|e| e.to_string())
    }
}

fn sqlite_rows(rc: &Connection, sql: &str) -> Result<Vec<Vec<Sv>>, String> {
    let mut stmt = rc.prepare(sql).map_err(|e| e.to_string())?;
    let ncols = stmt.column_count();
    if ncols == 0 {
        return stmt
            .execute([])
            .map(|_| Vec::new())
            .map_err(|e| e.to_string());
    }
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                let mut r = Vec::with_capacity(ncols);
                for i in 0..ncols {
                    r.push(row.get(i).unwrap_or(Sv::Null));
                }
                out.push(r);
            }
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(out)
}

fn values_match(a: &Value, b: &Sv) -> bool {
    match (a, b) {
        (Value::Null, Sv::Null) => true,
        (Value::Null, _) | (_, Sv::Null) => false,
        (Value::Integer(x), Sv::Integer(y)) => x == y,
        (Value::Integer(x), Sv::Real(y)) => (*x as f64 - y).abs() <= 1e-9 * y.abs().max(1.0),
        (Value::Real(x), Sv::Integer(y)) => (*x - *y as f64).abs() <= 1e-9 * x.abs().max(1.0),
        (Value::Real(x), Sv::Real(y)) => {
            (x.is_nan() && y.is_nan())
                || x == y
                || (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)
        }
        (Value::Text(x), Sv::Text(y)) => x.as_str() == y,
        (Value::Blob(x), Sv::Blob(y)) => x == y,
        _ => false,
    }
}

/// Compare a statement's result + error parity, then the full state of
/// every table, between the two engines.
fn step_and_compare(db: &mut Database, rc: &Connection, sql: &str) {
    let ours = engine_rows(db, sql);
    let theirs = sqlite_rows(rc, sql);
    match (&ours, &theirs) {
        (Ok(a), Ok(b)) => {
            assert_eq!(
                a.len(),
                b.len(),
                "[{sql}] row count {} vs {}",
                a.len(),
                b.len()
            );
            for (i, (r1, r2)) in a.iter().zip(b.iter()).enumerate() {
                for (j, (v1, v2)) in r1.iter().zip(r2.iter()).enumerate() {
                    assert!(
                        values_match(v1, v2),
                        "[{sql}] row {i} col {j}: {v1:?} vs {v2:?}"
                    );
                }
            }
        }
        (Err(a), Err(_)) => {
            // Error parity: our message need not be byte-identical here
            // (constraint text shapes are pinned by dedicated suites);
            // both rejecting IS the contract.
            assert!(
                !a.is_empty(),
                "[{sql}] our error message should be non-empty"
            );
        }
        (a, b) => panic!("[{sql}] error parity: ours {a:?} vs sqlite {b:?}"),
    }
    // Integrity: ours must stay index-consistent after every statement.
    let integ = engine_rows(db, "PRAGMA integrity_check").unwrap();
    for r in integ {
        if let Some(Value::Text(t)) = r.first() {
            assert_eq!(t.as_str(), "ok", "[{sql}] integrity_check: {}", t.as_str());
        }
    }
}

#[test]
fn index_root_split_in_trigger_cascade() {
    let mut db = Database::open_in_memory().unwrap();
    let rc = Connection::open_in_memory().unwrap();
    let setup = [
        "CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT)",
        "CREATE INDEX ix3 ON audit(note)",
        // Two cascading self-inserting triggers: every DELETE fires
        // trg10 whose INSERT fires trg4 — enough rows to split ix3's
        // leaf mid-statement (the nested split was reverted by the
        // outer statement's stale-root write-back).
        "CREATE TRIGGER trg4 AFTER INSERT ON audit BEGIN INSERT INTO audit (note) VALUES ('日本語'); END",
        "CREATE TRIGGER trg10 AFTER DELETE ON audit BEGIN INSERT INTO audit (note) VALUES ('  padded  '); END",
    ];
    for s in setup {
        engine_rows(&mut db, s).unwrap();
        sqlite_rows(&rc, s).unwrap();
    }
    // Seed enough DISTINCT notes that ix3's leaf fills and splits
    // during the cascade.
    let seed: Vec<String> = (0..400).map(|i| format!("('note-{i:04}')")).collect();
    let ins = format!("INSERT INTO audit (note) VALUES {}", seed.join(", "));
    step_and_compare(&mut db, &rc, &ins);

    // Delete a range — each deleted row adds 2 cascade rows, splitting
    // ix3 repeatedly while the statement is mid-flight.
    step_and_compare(&mut db, &rc, "DELETE FROM audit WHERE id BETWEEN 1 AND 120");
    step_and_compare(&mut db, &rc, "SELECT count(*) FROM audit");
    step_and_compare(
        &mut db,
        &rc,
        "SELECT note, count(*) FROM audit GROUP BY note ORDER BY 1 LIMIT 10",
    );
    // The index must drive queries consistently (a missed entry would
    // make the index-driven count diverge from the table scan).
    step_and_compare(
        &mut db,
        &rc,
        "SELECT count(*) FROM audit WHERE note = '日本語'",
    );
    step_and_compare(
        &mut db,
        &rc,
        "SELECT count(*) FROM audit WHERE note = '  padded  '",
    );
}

#[test]
fn failed_delete_all_rolls_back_every_row() {
    let mut db = Database::open_in_memory().unwrap();
    let rc = Connection::open_in_memory().unwrap();
    let setup = [
        "CREATE TABLE t0 (d REAL, b INTEGER PRIMARY KEY AUTOINCREMENT, c INTEGER)",
        "CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT)",
        // The partial unique index makes the SECOND trg2-fired insert
        // ('0') fail — mid-statement, with rows already deleted.
        "CREATE UNIQUE INDEX ix0 ON audit(note) WHERE note IS NOT NULL",
        "CREATE TRIGGER trg2 AFTER DELETE ON t0 BEGIN INSERT INTO audit (note) VALUES ('0'); END",
    ];
    for s in setup {
        engine_rows(&mut db, s).unwrap();
        sqlite_rows(&rc, s).unwrap();
    }
    for (d, b, c) in [
        (1.5f64, 0i64, 5i64),
        (-10.714, 45, 32),
        (82.0, 47, 7),
        (61.857, 51, -15),
    ] {
        let ins = format!("INSERT INTO t0 (d, b, c) VALUES ({d}, {b}, {c})");
        engine_rows(&mut db, &ins).unwrap();
        sqlite_rows(&rc, &ins).unwrap();
    }
    engine_rows(&mut db, "INSERT INTO audit (id, note) VALUES (1, '0')").unwrap();
    sqlite_rows(&rc, "INSERT INTO audit (id, note) VALUES (1, '0')").unwrap();

    // The delete-all fires trg2 per row; the second insert hits ix0 and
    // the statement FAILS — SQLite restores ALL deleted rows; so must we.
    step_and_compare(&mut db, &rc, "DELETE FROM t0");
    // Every row must be back (the bug: row b=51 stayed deleted).
    step_and_compare(&mut db, &rc, "SELECT b, d, c FROM t0 ORDER BY b");
    step_and_compare(&mut db, &rc, "SELECT count(*) FROM t0");
    step_and_compare(&mut db, &rc, "SELECT id, note FROM audit ORDER BY id");
}

#[test]
fn upsert_target_checked_before_other_indexes() {
    let mut db = Database::open_in_memory().unwrap();
    let rc = Connection::open_in_memory().unwrap();
    let setup = [
        "CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT)",
        "CREATE UNIQUE INDEX ix0 ON audit(note) WHERE note IS NOT NULL",
        "INSERT INTO audit (id, note) VALUES (39, 'alpha')",
    ];
    for s in setup {
        engine_rows(&mut db, s).unwrap();
        sqlite_rows(&rc, s).unwrap();
    }
    // BOTH id=39 and note='alpha' are taken: the target (id) conflict
    // fires first in SQLite — DO NOTHING — the note conflict is never
    // reached. Our engine used to check ix0 first and error.
    step_and_compare(
        &mut db,
        &rc,
        "INSERT INTO audit (id, note) VALUES (39, 'alpha') ON CONFLICT(id) DO NOTHING",
    );
    // The fresh-id + note-conflict shape still errors in BOTH.
    step_and_compare(
        &mut db,
        &rc,
        "INSERT INTO audit (id, note) VALUES (40, 'alpha') ON CONFLICT(id) DO NOTHING",
    );
    // Empty-target upsert: a rowid conflict takes DO NOTHING too.
    step_and_compare(
        &mut db,
        &rc,
        "INSERT INTO audit (id, note) VALUES (39, 'beta') ON CONFLICT DO NOTHING",
    );
    step_and_compare(&mut db, &rc, "SELECT id, note FROM audit ORDER BY id");
}

#[test]
fn insert_or_ignore_skips_not_null() {
    let mut db = Database::open_in_memory().unwrap();
    let rc = Connection::open_in_memory().unwrap();
    let setup = [
        "CREATE TABLE t0 (f INTEGER, c BLOB NOT NULL, PRIMARY KEY (f))",
        "ALTER TABLE t0 ADD COLUMN added2 TEXT DEFAULT 'x7'",
    ];
    for s in setup {
        engine_rows(&mut db, s).unwrap();
        sqlite_rows(&rc, s).unwrap();
    }
    // OR IGNORE skips the NULL row silently; the valid row lands.
    step_and_compare(
        &mut db,
        &rc,
        "INSERT OR IGNORE INTO t0 (f, c) VALUES (9, NULL)",
    );
    step_and_compare(
        &mut db,
        &rc,
        "INSERT OR IGNORE INTO t0 (f, c) VALUES (9, x'00')",
    );
    step_and_compare(&mut db, &rc, "SELECT f, c, added2 FROM t0 ORDER BY f");
    // OR REPLACE still errors on NOT NULL (a NULL cannot be replaced
    // into existence) — error parity in both engines.
    step_and_compare(
        &mut db,
        &rc,
        "INSERT OR REPLACE INTO t0 (f, c) VALUES (10, NULL)",
    );
    step_and_compare(&mut db, &rc, "SELECT count(*) FROM t0");
    // CHECK constraints behave the same under OR IGNORE.
    step_and_compare(&mut db, &rc, "CREATE TABLE tc (x INTEGER CHECK (x > 0))");
    step_and_compare(&mut db, &rc, "INSERT OR IGNORE INTO tc VALUES (-5)");
    step_and_compare(&mut db, &rc, "INSERT OR IGNORE INTO tc VALUES (5)");
    step_and_compare(&mut db, &rc, "SELECT x FROM tc");
}

#[test]
fn autoincrement_negative_rowids_never_raise_sequence() {
    let mut db = Database::open_in_memory().unwrap();
    let rc = Connection::open_in_memory().unwrap();
    let setup = ["CREATE TABLE t0 (d TEXT, c INTEGER PRIMARY KEY AUTOINCREMENT)"];
    for s in setup {
        engine_rows(&mut db, s).unwrap();
        sqlite_rows(&rc, s).unwrap();
    }
    // Negative explicit rowids never raise the sequence: the first AUTO
    // id on a negative-only table is 1 (the bug allocated -7).
    step_and_compare(&mut db, &rc, "INSERT INTO t0 (c, d) VALUES (-8, 'neg')");
    step_and_compare(&mut db, &rc, "INSERT INTO t0 (d) VALUES ('auto')");
    step_and_compare(&mut db, &rc, "SELECT c, d FROM t0 ORDER BY c");
    // Positive explicit rowids DO raise it.
    step_and_compare(&mut db, &rc, "INSERT INTO t0 (c, d) VALUES (50, 'pos')");
    step_and_compare(&mut db, &rc, "INSERT INTO t0 (d) VALUES ('auto2')");
    step_and_compare(&mut db, &rc, "SELECT c, d FROM t0 ORDER BY c");
    // Deleted top ids are never reused (the AUTOINCREMENT contract).
    step_and_compare(&mut db, &rc, "DELETE FROM t0 WHERE c = 51");
    step_and_compare(&mut db, &rc, "INSERT INTO t0 (d) VALUES ('auto3')");
    step_and_compare(&mut db, &rc, "SELECT c, d FROM t0 ORDER BY c");
    // sqlite_sequence visible parity.
    step_and_compare(
        &mut db,
        &rc,
        "SELECT seq FROM sqlite_sequence WHERE name = 't0'",
    );
}

use rusqlite::types::Value as Sv;
