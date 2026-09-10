//! Cell-mode step-path tests: the zero-materialize serving layer where the
//! streaming drivers hand RAW record bytes to the statement, which decodes
//! each row into ONE reused serve buffer (`Statement::step`'s cell mode —
//! the fused `ProjectedScanDriver` / `ProjectedRangeDriver` shapes).
//!
//! Contract: for every shape the drivers can serve, the step path must
//! return IDENTICAL rows (values, column names, order) to the materialized
//! `query()` path — across batch boundaries (1024-row pulls), projection
//! permutations, duplicates, NULLs, overflow rows, rowid sentinels,
//! re-binding, take_row interleave, and fallback shapes (LIMIT / Filter).

use rustqlite::types::Value;
use rustqlite::{Database, StepResult};

/// One batch is 1024 rows (SCAN_BATCH_ROWS): build tables that straddle
/// 1, 2, and 3+ batch boundaries, plus the exact-boundary case.
fn rich_db(rows: i64) -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL, data BLOB)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    let mut i = 0i64;
    while i < rows {
        let hi = (i + 500).min(rows);
        let mut sql = String::from("INSERT INTO t (id, name, val, score, data) VALUES ");
        for j in i..hi {
            if j > i {
                sql.push(',');
            }
            // Adversarial values: NULLs, negatives, non-ASCII text,
            // astral-plane chars, blobs, integral reals.
            let name = match j % 7 {
                0 => "NULL".to_string(),
                1 => format!("'n-{}'", j),
                2 => format!("'日本語-{}'", j),
                3 => format!("'emoji-𝒜𝒵-{}'", j),
                4 => "NULL".to_string(),
                5 => format!("'x{}'", j),
                _ => format!("'pad-{}-pad'", j),
            };
            let val = if j % 11 == 3 {
                "NULL".to_string()
            } else {
                (j - rows / 2).to_string()
            };
            let score = if j % 13 == 5 {
                "NULL".to_string()
            } else {
                format!("{}.5", j)
            };
            let data = match j % 5 {
                0 => "NULL".to_string(),
                1 => format!("X'{}'", hex_of(&(j as u32).to_be_bytes())),
                _ => format!("X'{}'", hex_of(&vec![j as u8; (j % 9 + 1) as usize])),
            };
            sql.push_str(&format!("({}, {}, {}, {}, {})", j, name, val, score, data));
        }
        db.execute(&sql, []).unwrap();
        i = hi;
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02X}", b)).collect()
}

/// Step a statement to completion, collecting rows via the row() accessor.
fn step_rows(db: &Database, sql: &str, binds: &[Value]) -> Vec<Vec<Value>> {
    let mut stmt = db.prepare(sql).unwrap();
    for (i, v) in binds.iter().enumerate() {
        stmt.bind(i + 1, v.clone()).unwrap();
    }
    let mut out = Vec::new();
    while stmt.step().unwrap() == StepResult::Row {
        let row: Vec<Value> = stmt.row().expect("row between Row and Done").clone();
        out.push(row);
    }
    out
}

fn assert_same_as_query(sql: &str, binds: &[Value]) {
    let db = rich_db(3000);
    let stepped = step_rows(&db, sql, binds);
    let materialized = db.query(sql, binds.to_vec()).unwrap();
    assert_eq!(
        stepped, materialized,
        "cell-mode step rows must equal query() rows for {sql}"
    );
}

// ---------------------------------------------------------------- ranges

#[test]
fn cell_range_matches_query() {
    assert_same_as_query(
        "SELECT id, val FROM t WHERE id BETWEEN 1 AND ?",
        &[Value::Integer(2500)],
    );
    // Empty range (lo > hi).
    assert_same_as_query("SELECT id, val FROM t WHERE id BETWEEN 10 AND 5", &[]);
    // Single row.
    assert_same_as_query("SELECT id, val FROM t WHERE id BETWEEN 42 AND 42", &[]);
    // Open-ended top (end = MAX): drains to EOF through batch pulls.
    assert_same_as_query(
        "SELECT id, val FROM t WHERE id >= ?",
        &[Value::Integer(2990)],
    );
    // Parameterized both sides.
    assert_same_as_query(
        "SELECT id, val FROM t WHERE id BETWEEN ? AND ?",
        &[Value::Integer(700), Value::Integer(2000)],
    );
}

#[test]
fn cell_range_spans_batch_boundaries() {
    // 1024 = SCAN_BATCH_ROWS exactly; 1025 forces a third pull with one
    // row; 3000 exercises three pulls plus a remainder.
    for span in [1i64, 500, 1023, 1024, 1025, 2048, 3000] {
        let db = rich_db(3000);
        let stepped = step_rows(
            &db,
            "SELECT id FROM t WHERE id BETWEEN 0 AND ?",
            &[Value::Integer(span - 1)],
        );
        assert_eq!(stepped.len() as i64, span, "span {span} row count");
        for (i, row) in stepped.iter().enumerate() {
            assert_eq!(row[0], Value::Integer(i as i64), "span {span} row {i}");
        }
    }
}

// ------------------------------------------------------------ projections

#[test]
fn cell_projection_reorder_and_duplicates() {
    // Non-ascending projection takes the permutation path in the decoder.
    assert_same_as_query(
        "SELECT val, id FROM t WHERE id BETWEEN 1 AND ?",
        &[Value::Integer(900)],
    );
    // Duplicated columns hit the run placement.
    assert_same_as_query(
        "SELECT val, val, id FROM t WHERE id BETWEEN 1 AND ?",
        &[Value::Integer(900)],
    );
    // Star projection (all columns).
    assert_same_as_query(
        "SELECT * FROM t WHERE id BETWEEN 1 AND ?",
        &[Value::Integer(40)],
    );
    // Wide + text + blob mix.
    assert_same_as_query(
        "SELECT name, val, score, data FROM t WHERE id BETWEEN 1 AND ?",
        &[Value::Integer(1200)],
    );
}

#[test]
fn cell_rowid_sentinel_and_alias() {
    // id is the INTEGER PRIMARY KEY alias: all-rowid fast path.
    let db = rich_db(500);
    let rows = step_rows(&db, "SELECT id FROM t WHERE id BETWEEN 10 AND 19", &[]);
    assert_eq!(rows.len(), 10);
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r[0], Value::Integer(10 + i as i64));
    }

    // A table WITHOUT a rowid alias: `rowid` resolves through the
    // ROWID_PROJ sentinel.
    let mut db2 = Database::open_in_memory().unwrap();
    db2.execute("CREATE TABLE u (a TEXT, b INTEGER)", [])
        .unwrap();
    db2.execute("INSERT INTO u (a, b) VALUES ('x', 1), ('y', 2)", [])
        .unwrap();
    let rows = step_rows(&db2, "SELECT rowid, a, b FROM u", &[]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Integer(1));
    assert_eq!(rows[1][0], Value::Integer(2));
    assert_eq!(rows[0][1], Value::Text("x".into()));

    // rowid + rowid alias both, on the alias table.
    let rows = step_rows(&db, "SELECT rowid, id FROM t WHERE id BETWEEN 3 AND 5", &[]);
    assert_eq!(rows.len(), 3);
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r[0], Value::Integer(3 + i as i64));
        assert_eq!(r[1], Value::Integer(3 + i as i64));
    }
}

// ---------------------------------------------------- short rows + NULLs

#[test]
fn cell_short_rows_null_fill() {
    // Rows inserted with FEWER columns than the projection wants: the
    // decoder's trailing null-fill path (NULL defaults).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE s (a, b, c, d)", []).unwrap();
    db.execute("INSERT INTO s (a) VALUES (1)", []).unwrap();
    db.execute("INSERT INTO s (a, b) VALUES (2, 'x')", [])
        .unwrap();
    db.execute("INSERT INTO s (a, b, c) VALUES (3, 'y', 9)", [])
        .unwrap();
    db.execute("INSERT INTO s VALUES (4, 'z', 8, 7.5)", [])
        .unwrap();
    let stepped = step_rows(&db, "SELECT a, b, c, d FROM s", &[]);
    let materialized = db.query("SELECT a, b, c, d FROM s", []).unwrap();
    assert_eq!(stepped, materialized);
    assert_eq!(
        stepped[0],
        vec![Value::Integer(1), Value::Null, Value::Null, Value::Null]
    );
    assert_eq!(
        stepped[2],
        vec![
            Value::Integer(3),
            Value::Text("y".into()),
            Value::Integer(9),
            Value::Null
        ]
    );
}

// ------------------------------------------------------------ wide rows

#[test]
fn cell_overflow_rows_in_range() {
    // Payloads past a page reassemble through the overflow chain into
    // the arena — wide TEXT and BLOB rows in the middle of a range.
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE w (id INTEGER PRIMARY KEY, big TEXT, tail INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=50i64 {
        let big = if i % 3 == 0 {
            format!("'{}'", "A".repeat(9000)) // ~9 KB: multi-page chain
        } else {
            format!("'small-{}'", i)
        };
        db.execute(
            &format!(
                "INSERT INTO w (id, big, tail) VALUES ({}, {}, {})",
                i,
                big,
                i * 2
            ),
            [],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let stepped = step_rows(
        &db,
        "SELECT id, big, tail FROM w WHERE id BETWEEN 1 AND 50",
        &[],
    );
    let materialized = db
        .query("SELECT id, big, tail FROM w WHERE id BETWEEN 1 AND 50", [])
        .unwrap();
    assert_eq!(stepped.len(), 50);
    for (i, row) in stepped.iter().enumerate() {
        assert_eq!(row[0], Value::Integer(i as i64 + 1));
        assert_eq!(row[2], Value::Integer(((i + 1) * 2) as i64));
        if (i + 1) % 3 == 0 {
            let expected = "A".repeat(9000);
            assert_eq!(row[1], Value::Text(expected.into()));
        }
    }
    // The materialized path comparison (same values).
    assert_eq!(stepped, materialized);
}

// ---------------------------------------------------- accessors + take_row

#[test]
fn cell_accessors_and_take_row() {
    let db = rich_db(2000);
    let mut stmt = db
        .prepare("SELECT id, name, val, score, data FROM t WHERE id BETWEEN 100 AND 300")
        .unwrap();
    let mut count = 0;
    while stmt.step().unwrap() == StepResult::Row {
        let id = stmt.column_int(0);
        // Accessors read the serve buffer.
        if let Some(v) = stmt.column_value(2) {
            if v.as_integer() % 11 == 3 {
                assert!(matches!(v, Value::Null));
            }
        }
        // row() and column_value() see the SAME row.
        let via_row = stmt.row().unwrap().clone();
        let via_col = stmt.column_value(0).unwrap().clone();
        assert_eq!(via_row[0], via_col);
        // take_row mid-stream: the next accessor sees None until the
        // next step, then rows continue.
        if id == 150 {
            let taken = stmt.take_row().expect("take mid-stream");
            assert_eq!(taken[0], Value::Integer(150));
            assert!(stmt.row().is_none());
            assert!(stmt.column_value(0).is_none());
        }
        count += 1;
    }
    assert_eq!(count, 201);
}

#[test]
fn cell_query_all_and_columns() {
    let db = rich_db(1500);
    let mut stmt = db
        .prepare("SELECT val, id FROM t WHERE id BETWEEN 10 AND 1100")
        .unwrap();
    // Column names are known at PREPARE time for driver shapes.
    assert_eq!(stmt.column_count(), 2);
    assert_eq!(stmt.column_name(0), Some("val"));
    assert_eq!(stmt.column_name(1), Some("id"));
    let rows = stmt.query_all().unwrap();
    assert_eq!(rows.len(), 1091);
    assert_eq!(rows[0][1], Value::Integer(10));
    let materialized = db
        .query("SELECT val, id FROM t WHERE id BETWEEN 10 AND 1100", [])
        .unwrap();
    assert_eq!(rows, materialized);
}

// ------------------------------------------------------------ rebinding

#[test]
fn cell_reset_rebind_rerun() {
    let db = rich_db(2000);
    let mut stmt = db
        .prepare("SELECT id, val FROM t WHERE id BETWEEN ? AND ?")
        .unwrap();
    for (lo, hi) in [(0i64, 99), (500, 1500), (1990, 1999), (0, 0), (700, 699)] {
        stmt.reset();
        stmt.bind(1, Value::Integer(lo)).unwrap();
        stmt.bind(2, Value::Integer(hi)).unwrap();
        let mut ids = Vec::new();
        while stmt.step().unwrap() == StepResult::Row {
            ids.push(stmt.column_int(0));
        }
        let expect: Vec<i64> = (lo..=hi).collect();
        assert_eq!(ids, expect, "range {lo}..={hi}");
    }
}

#[test]
fn cell_dml_between_executions() {
    let mut db = rich_db(500);
    let mut first = Vec::new();
    {
        let mut stmt = db
            .prepare("SELECT id FROM t WHERE id BETWEEN 0 AND 9999")
            .unwrap();
        while stmt.step().unwrap() == StepResult::Row {
            first.push(stmt.column_int(0));
        }
    }
    assert_eq!(first.len(), 500);
    // Write after the statement finalized; a fresh statement sees the new row.
    db.execute("INSERT INTO t (id, name, val) VALUES (900, 'new', 1)", [])
        .unwrap();
    let mut second = Vec::new();
    {
        let mut stmt = db
            .prepare("SELECT id FROM t WHERE id BETWEEN 0 AND 9999")
            .unwrap();
        while stmt.step().unwrap() == StepResult::Row {
            second.push(stmt.column_int(0));
        }
    }
    assert_eq!(second.len(), 501);
    assert_eq!(*second.last().unwrap(), 900);
}

// ------------------------------------------------------- fallback shapes

#[test]
fn cell_fallback_limit_and_filter() {
    // LIMIT wraps the driver: the wrapper refuses cell serving, the
    // materialized path serves — results identical.
    assert_same_as_query(
        "SELECT id, val FROM t WHERE id BETWEEN 0 AND ? LIMIT 55 OFFSET 10",
        &[Value::Integer(3000)],
    );
    // A non-rowid filter: FilteredScanDriver (fused, materialized).
    assert_same_as_query(
        "SELECT id, val FROM t WHERE val > ?",
        &[Value::Integer(1000)],
    );
    // Aggregate: never a driver shape.
    assert_same_as_query(
        "SELECT COUNT(*), SUM(val), MIN(score), MAX(id) FROM t WHERE id BETWEEN 0 AND ?",
        &[Value::Integer(2000)],
    );
    // ORDER BY: materialized sort on top.
    assert_same_as_query(
        "SELECT id, val FROM t WHERE id BETWEEN 0 AND ? ORDER BY val DESC",
        &[Value::Integer(500)],
    );
}

// ------------------------------------------------------------ file pager

#[test]
fn cell_mode_file_backed_database() {
    // mem_pager = false: the serve decode keeps full UTF-8 validation
    // (no trusted-mode arming) — identical values.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cell_mode.db");
    let mut db = Database::open(path.to_str().unwrap()).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    let mut i = 0i64;
    while i < 2100 {
        let hi = (i + 700).min(2100);
        let mut sql = String::from("INSERT INTO t (id, name, val) VALUES ");
        for j in i..hi {
            if j > i {
                sql.push(',');
            }
            sql.push_str(&format!("({}, 'nᵔ{}-π', {})", j, j, -j));
        }
        db.execute(&sql, []).unwrap();
        i = hi;
    }
    db.execute("COMMIT", []).unwrap();
    let stepped = step_rows(
        &db,
        "SELECT id, name, val FROM t WHERE id BETWEEN 0 AND 2099",
        &[],
    );
    assert_eq!(stepped.len(), 2100);
    for (i, row) in stepped.iter().enumerate() {
        assert_eq!(row[1], Value::Text(format!("nᵔ{}-π", i).into()));
        assert_eq!(row[2], Value::Integer(-(i as i64)));
    }
}

// ------------------------------------------------------- column_* typing

#[test]
fn cell_column_typed_accessors() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE ty (id INTEGER PRIMARY KEY, t TEXT, r REAL, b BLOB)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO ty (t, r, b) VALUES ('hello', 2.5, X'DEADBEEF')",
        [],
    )
    .unwrap();
    let mut stmt = db
        .prepare("SELECT id, t, r, b FROM ty WHERE id = 1")
        .unwrap();
    assert_eq!(stmt.step().unwrap(), StepResult::Row);
    assert_eq!(stmt.column_int(0), 1);
    assert_eq!(stmt.column_text(1).unwrap(), "hello");
    assert_eq!(stmt.column_real(2), 2.5);
    assert_eq!(stmt.column_blob(3).unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    // column_text on an integer coerces like SQLite's value accessors.
    assert_eq!(stmt.step().unwrap(), StepResult::Done);
}
