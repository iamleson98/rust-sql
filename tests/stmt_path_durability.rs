//! Regression tests for the prepared-statement (prepare/step) DML paths,
//! pinned after two historically-found bugs:
//!
//! 1. **UPDATE change accounting** — the single-row OLTP fast path
//!    (`UPDATE t SET ... WHERE id = ?`) applied the row but returned
//!    without bumping `ctx.changes`, so `changes()` / `total_changes()`
//!    (and `sqlite3_changes` through the C ABI, which sqlx surfaces as
//!    `rows_affected()`) all reported 0 — sea-orm turns that into
//!    `RecordNotUpdated`. DELETE's fast paths counted correctly; only
//!    UPDATE's single-row arm missed it.
//!
//! 2. **SQLite-format durability through statements** — the streaming
//!    statement path bypassed `Database::execute`, so DML committed via
//!    prepare/step on a SQLite-format (foreign) database never reached
//!    `dump_foreign`: rows lived only in memory, the Drop checkpoint
//!    published a stale image, and the WAL sidecar was deleted (total
//!    data loss on close). The statement epilogue now mirrors
//!    `note_foreign_write` (mark dirty + dump at autocommit boundaries),
//!    so every prepared DML commit appends real WAL frames — exactly
//!    what SQLite itself does.

use rustqlite::{Database, StepResult, Value};
use std::path::PathBuf;

fn temp_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rsql_stmt_dura_{}_{}", name, std::process::id()));
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", p.display(), suffix));
    }
    p
}

fn step_all(db: &mut Database, sql: &str, params: &[Value]) {
    let mut stmt = db.prepare(sql).unwrap();
    stmt.bind_all(params).unwrap();
    while stmt.step().unwrap() == StepResult::Row {}
    stmt.finalize().unwrap();
}

// ---------------------------------------------------------------------------
// 1. UPDATE change accounting
// ---------------------------------------------------------------------------

#[test]
fn update_changes_counter_all_paths() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT NOT NULL)",
        [],
    )
    .unwrap();

    // Baseline: INSERT counts (this already worked).
    step_all(&mut db, "INSERT INTO t (v) VALUES (?)", &[Value::Text("a".into())]);
    assert_eq!(db.changes(), 1, "INSERT via statement must count");

    // The bug: single-row UPDATE fast path returned without counting.
    step_all(
        &mut db,
        "UPDATE t SET v = ? WHERE id = ?",
        &[Value::Text("b".into()), Value::Integer(1)],
    );
    assert_eq!(db.changes(), 1, "UPDATE via statement must count (fast path)");
    let total_after_one = db.total_changes();

    // Literal form (same fast path, no binds).
    db.execute("UPDATE t SET v = 'c' WHERE id = 1", []).unwrap();
    assert_eq!(db.changes(), 1, "UPDATE via execute must count");

    // Zero-row UPDATE reports 0, not the previous statement's count.
    db.execute("UPDATE t SET v = 'x' WHERE id = 999", []).unwrap();
    assert_eq!(db.changes(), 0, "zero-row UPDATE must report 0");

    // Multi-row range UPDATE through the statement path.
    for v in ["a", "b", "c"] {
        step_all(&mut db, "INSERT INTO t (v) VALUES (?)", &[Value::Text(v.into())]);
    }
    step_all(&mut db, "UPDATE t SET v = 'z' WHERE id >= ?", &[Value::Integer(2)]);
    assert_eq!(db.changes(), 3, "range UPDATE must count every row");
    assert_eq!(
        db.total_changes() - total_after_one,
        7,
        "total_changes accumulates (1 execute UPDATE + 3 INSERTs + 3 range UPDATE)"
    );

    // DELETE keeps counting (regression guard for the neighboring path).
    db.execute("DELETE FROM t WHERE id = 1", []).unwrap();
    assert_eq!(db.changes(), 1);
}

// ---------------------------------------------------------------------------
// 2. SQLite-format durability through the statement path
// ---------------------------------------------------------------------------

#[test]
fn sqlite_format_stmt_path_durable() {
    let path = temp_path("stmt_dura");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        assert_eq!(db.disk_format(), "sqlite");
        // DDL through execute (the compat layer's Once path)…
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT NOT NULL)",
            [],
        )
        .unwrap();
        // …then EVERY write through prepare/step ONLY — the exact shape
        // that lost data before the fix (sqlx's prepared DML).
        for v in ["a", "b", "c"] {
            step_all(&mut db, "INSERT INTO t (v) VALUES (?)", &[Value::Text(v.into())]);
        }
        step_all(
            &mut db,
            "UPDATE t SET v = ? WHERE id = ?",
            &[Value::Text("z".into()), Value::Integer(2)],
        );
        step_all(&mut db, "DELETE FROM t WHERE id = ?", &[Value::Integer(3)]);
    } // Drop here — the pre-fix Drop checkpoint wrote a stale image.

    // Engine reopen: rows survive.
    {
        let db = Database::open(&path).unwrap();
        assert_eq!(db.disk_format(), "sqlite");
        let rows = db
            .query("SELECT id, v FROM t ORDER BY id", [])
            .unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Text("a".into())],
                vec![Value::Integer(2), Value::Text("z".into())],
            ],
            "statement-path DML must survive close/reopen"
        );
    }

    // Real SQLite verifies the file the engine wrote.
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        let ok: String = con
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
        let rows: Vec<(i64, String)> = {
            let mut stmt = con.prepare("SELECT id, v FROM t ORDER BY id").unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            rows,
            vec![(1, "a".to_string()), (2, "z".to_string())],
            "real SQLite must read what the statement path wrote"
        );
    }
}

#[test]
fn sqlite_format_stmt_path_tx_commit_durable() {
    let path = temp_path("stmt_tx");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT NOT NULL)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for v in ["x", "y"] {
            step_all(&mut db, "INSERT INTO t (v) VALUES (?)", &[Value::Text(v.into())]);
        }
        // Rows inside the open transaction are NOT autocommit-dumped…
        db.execute("COMMIT", []).unwrap();
        // …COMMIT is the dump boundary.
    }
    let db = Database::open(&path).unwrap();
    let rows = db.query("SELECT v FROM t ORDER BY id", []).unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::Text("x".into())], vec![Value::Text("y".into())]],
        "statement-path DML inside BEGIN/COMMIT must survive"
    );
}
