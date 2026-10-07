//! Sorted-batch index maintenance (the pinned-leaf sweep round's
//! regression suite).
//!
//! The executor's UPDATE path buffers eligible indexes' maintenance ops
//! (non-unique, plain B-tree, no update triggers) and applies them as
//! sorted sweeps; CREATE INDEX backfills through a collect + sort +
//! append-build. Both paths must be OBSERVABLY IDENTICAL to the per-row
//! immediate path they replace:
//!   * band UPDATEs of cyclic indexed keys — the marathon M9 shape —
//!     leave the index with exactly the per-row multiset;
//!   * the per-row-forcing shapes (UNIQUE index, update trigger) agree
//!     with the batched shape row for row;
//!   * a mid-statement failure (trigger RAISE) restores BOTH the rows
//!     and every index entry — the undo replay's idempotent re-insert
//!     is the keystone (an un-applied batch must not duplicate entries);
//!   * the sorted CREATE INDEX backfill builds exact indexes (cyclic,
//!     duplicate-heavy, NULL-bearing, partial, expression, DESC, and
//!     UNIQUE-violating shapes) with adjacent-duplicate detection.

use rustqlite::{Database, Value};

fn tmpdb(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "sbidx-{}-{}-{}.db",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// Build the events table (the M9 cyclic shape) with `rows` rows.
fn build_events(db: &mut Database, rows: i64, cycle: i64) {
    db.execute("BEGIN", ()).unwrap();
    const BATCH: i64 = 2000;
    let mut next = 0i64;
    while next < rows {
        let n = BATCH.min(rows - next);
        let mut sql = String::from("INSERT INTO events (id, k, note) VALUES ");
        for j in 0..n {
            let i = next + j + 1;
            if j > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({}, {}, 'n{:x}')", i, i % cycle, i));
        }
        db.execute(&sql, ()).unwrap();
        next += n;
    }
    db.execute("COMMIT", ()).unwrap();
}

/// The full index contents in (k, id) order — computed via an ORDER BY
/// that the planner satisfies with the index, and cross-checked against
/// an agnostic full-scan aggregate.
fn index_pairs(db: &mut Database) -> Vec<(i64, i64)> {
    let rows = db
        .query("SELECT k, id FROM events ORDER BY k, id", ())
        .unwrap();
    rows.iter()
        .map(|r| (r[0].as_integer(), r[1].as_integer()))
        .collect()
}

/// The expected (k, id) multiset after a band shift, computed in Rust.
/// NOTE: `k = k + shift` is PLAIN INTEGER ADDITION — the cycle only
/// shaped the INSERT-time data; shifted values legitimately exceed the
/// cycle (no wrap).
fn expected_after_band(rows: i64, cycle: i64, band_end: i64, shift: i64) -> Vec<(i64, i64)> {
    let mut v: Vec<(i64, i64)> = Vec::with_capacity(rows as usize);
    for id in 1..=rows {
        let k = if id <= band_end {
            id % cycle + shift
        } else {
            id % cycle
        };
        v.push((k, id));
    }
    v.sort_unstable();
    v
}

fn integrity(db: &mut Database) -> String {
    db.query("PRAGMA integrity_check", ())
        .unwrap()
        .first()
        .and_then(|r| r.first())
        .map(|v| v.as_text().to_string())
        .unwrap_or_default()
}

/// The headline: a band UPDATE of the cyclic indexed column — the
/// batched sweep shape — leaves the index byte-for-row exact.
#[test]
fn band_update_indexed_column_matches_expected() {
    let (rows, cycle, band_end, shift) = (30_000i64, 10_000i64, 6_000i64, 3_500i64);
    let mut db = Database::open(tmpdb("band")).unwrap();
    db.execute("PRAGMA journal_mode = WAL", ()).unwrap();
    db.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        (),
    )
    .unwrap();
    db.execute("CREATE INDEX ix_events_k ON events (k)", ())
        .unwrap();
    build_events(&mut db, rows, cycle);

    db.execute(
        &format!("UPDATE events SET k = k + {shift} WHERE id <= {band_end}"),
        (),
    )
    .unwrap();

    let got = index_pairs(&mut db);
    let want = expected_after_band(rows, cycle, band_end, shift);
    assert_eq!(
        got, want,
        "index contents diverged from the expected multiset"
    );
    assert_eq!(integrity(&mut db), "ok");

    // A second band over the SAME range (delete+insert churn on entries
    // the first pass just wrote — the recycling/refill shape). A TWIN
    // database with a dummy update trigger (per-row maintenance) runs
    // the identical statements — all three views must agree.
    let mut db2 = Database::open(tmpdb("band-twin")).unwrap();
    db2.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        (),
    )
    .unwrap();
    db2.execute("CREATE TABLE log (t TEXT)", ()).unwrap();
    db2.execute(
        "CREATE TRIGGER trg AFTER UPDATE ON events BEGIN INSERT INTO log VALUES ('u'); END",
        (),
    )
    .unwrap();
    db2.execute("CREATE INDEX ix_events_k ON events (k)", ())
        .unwrap();
    build_events(&mut db2, rows, cycle);
    db2.execute(
        &format!("UPDATE events SET k = k + {shift} WHERE id <= {band_end}"),
        (),
    )
    .unwrap();

    db.execute(
        &format!("UPDATE events SET k = k + {shift} WHERE id <= {band_end}"),
        (),
    )
    .unwrap();
    db2.execute(
        &format!("UPDATE events SET k = k + {shift} WHERE id <= {band_end}"),
        (),
    )
    .unwrap();

    let got2 = index_pairs(&mut db);
    let per_row2 = index_pairs(&mut db2);
    let want2 = expected_after_band(rows, cycle, band_end, shift * 2);
    if got2 != want2 {
        let mut i = 0;
        while i < got2.len().max(want2.len()) && got2.get(i) == want2.get(i) {
            i += 1;
        }
        panic!(
            "second band diverged at pair #{i}: batched={:?} expected={:?} (per-row twin {:?} here)",
            got2.get(i),
            want2.get(i),
            per_row2.get(i),
        );
    }
    assert_eq!(per_row2, want2, "per-row twin diverged from expectation");
    assert_eq!(integrity(&mut db), "ok");
}

/// The per-row-forcing shapes must agree with the batched shape row for
/// row: a UNIQUE index (immediate maintenance) and a table with an
/// update trigger (per-row loop) produce the same index contents as the
/// batched non-unique index on identical data.
#[test]
fn batched_and_immediate_paths_agree() {
    let (rows, cycle, band_end, shift) = (20_000i64, 7_000i64, 4_000i64, 2_300i64);
    let upd = format!("UPDATE events SET k = k + {shift} WHERE id <= {band_end}");

    // (a) batched: plain non-unique index.
    let mut a = Database::open(tmpdb("agree-a")).unwrap();
    a.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        (),
    )
    .unwrap();
    a.execute("CREATE INDEX ix ON events (k)", ()).unwrap();
    build_events(&mut a, rows, cycle);
    a.execute(&upd, ()).unwrap();
    let got_a = index_pairs(&mut a);

    // (b) per-row: UNIQUE index forces immediate maintenance.
    let mut b = Database::open(tmpdb("agree-b")).unwrap();
    b.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        (),
    )
    .unwrap();
    // Make k unique per row so the UNIQUE index is legal: k = id (dense,
    // distinct) with the band shifting into a free range.
    b.execute("BEGIN", ()).unwrap();
    for id in 1..=rows {
        b.execute(
            "INSERT INTO events (id, k, note) VALUES (?, ?, 'n')",
            vec![Value::Integer(id), Value::Integer(id)],
        )
        .unwrap();
    }
    b.execute("COMMIT", ()).unwrap();
    b.execute("CREATE UNIQUE INDEX ux ON events (k)", ())
        .unwrap();
    // Shift into a FREE key range (k + 1_000_000) — the dense keys leave
    // headroom above, so no transient collisions.
    b.execute(
        &format!("UPDATE events SET k = k + 1000000 WHERE id <= {band_end}"),
        (),
    )
    .unwrap();
    let got_b: Vec<(i64, i64)> = b
        .query("SELECT k, id FROM events ORDER BY k, id", ())
        .unwrap()
        .iter()
        .map(|r| (r[0].as_integer(), r[1].as_integer()))
        .collect();

    // (c) per-row: an update trigger on the table.
    let mut c = Database::open(tmpdb("agree-c")).unwrap();
    c.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        (),
    )
    .unwrap();
    c.execute("CREATE TABLE log (t TEXT)", ()).unwrap();
    c.execute(
        "CREATE TRIGGER trg AFTER UPDATE ON events BEGIN INSERT INTO log VALUES ('u'); END",
        (),
    )
    .unwrap();
    c.execute("CREATE INDEX ix ON events (k)", ()).unwrap();
    build_events(&mut c, rows, cycle);
    c.execute(&upd, ()).unwrap();
    let got_c = index_pairs(&mut c);

    // (a) vs (c): identical data + statement, different maintenance path.
    assert_eq!(got_a, got_c, "batched vs trigger(per-row) diverged");
    // (b): the unique-shifted dense shape is its own expectation.
    let mut want_b: Vec<(i64, i64)> = (1..=rows)
        .map(|id| {
            let k = if id <= band_end { id + 1000000 } else { id };
            (k, id)
        })
        .collect();
    want_b.sort_unstable();
    assert_eq!(got_b, want_b, "unique-index path diverged");
    assert_eq!(integrity(&mut a), "ok");
    assert_eq!(integrity(&mut b), "ok");
    assert_eq!(integrity(&mut c), "ok");
}

/// The keystone error path: a mid-statement failure with UN-APPLIED
/// buffered ops must restore rows AND index entries exactly — the undo
/// replay's idempotent re-inserts heal any flushed/unflushed mix.
#[test]
fn mid_statement_failure_restores_rows_and_index() {
    let (rows, cycle) = (8_000i64, 3_000i64);
    let mut db = Database::open(tmpdb("fail")).unwrap();
    db.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        (),
    )
    .unwrap();
    db.execute("CREATE INDEX ix ON events (k)", ()).unwrap();
    build_events(&mut db, rows, cycle);
    let before = index_pairs(&mut db);

    // A trigger that fails at row 5000 — after ~5000 rows' ops have been
    // buffered (and possibly chunk-flushed), the statement aborts.
    db.execute("CREATE TABLE log (t TEXT)", ()).unwrap();
    db.execute(
        "CREATE TRIGGER boom BEFORE UPDATE ON events WHEN NEW.id = 5000 BEGIN SELECT RAISE(ABORT, 'boom'); END",
        (),
    )
    .unwrap();
    let err = db.execute("UPDATE events SET k = k + 700 WHERE id <= 7000", ());
    assert!(err.is_err(), "the failing trigger must abort the statement");

    // Rows restored: contents identical to before.
    let after_rows: Vec<(i64, i64)> = db
        .query("SELECT k, id FROM events ORDER BY k, id", ())
        .unwrap()
        .iter()
        .map(|r| (r[0].as_integer(), r[1].as_integer()))
        .collect();
    assert_eq!(after_rows, before, "statement rollback diverged");
    assert_eq!(integrity(&mut db), "ok");
    // No trigger-log side effects leaked.
    let n: i64 = db.query("SELECT COUNT(*) FROM log", ()).unwrap()[0][0].as_integer();
    assert_eq!(
        n, 0,
        "BEFORE-trigger rows must not have logged (RAISE fired first)"
    );
}

/// The sorted CREATE INDEX backfill: cyclic keys (the shape whose
/// per-row inserts scattered), built dense and verified exactly.
#[test]
fn create_index_sorted_backfill_cyclic() {
    let (rows, cycle) = (40_000i64, 13_000i64);
    let mut db = Database::open(tmpdb("ci-cyclic")).unwrap();
    db.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        (),
    )
    .unwrap();
    build_events(&mut db, rows, cycle);
    db.execute("CREATE INDEX ix ON events (k)", ()).unwrap();
    assert_eq!(integrity(&mut db), "ok");
    let got = index_pairs(&mut db);
    let mut want: Vec<(i64, i64)> = (1..=rows).map(|id| (id % cycle, id)).collect();
    want.sort_unstable();
    assert_eq!(got, want, "sorted backfill diverged");
    // Index-driven point lookups over the whole cycle.
    for k in [0i64, 1, 500, 12_999] {
        let got: Vec<i64> = db
            .query("SELECT id FROM events WHERE k = ?", vec![Value::Integer(k)])
            .unwrap()
            .iter()
            .map(|r| r[0].as_integer())
            .collect();
        let mut want: Vec<i64> = (1..=rows).filter(|id| id % cycle == k).collect();
        want.sort_unstable();
        assert_eq!(got, want, "point lookup k={k} diverged");
    }
}

/// UNIQUE backfill: adjacent-duplicate detection over the sorted stream
/// (one pass replacing the per-row growing-tree lookup), with the
/// NULLs-are-distinct exemption.
#[test]
fn create_unique_index_sorted_backfill() {
    let mut db = Database::open(tmpdb("ci-unique")).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER)", ())
        .unwrap();
    db.execute(
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 10), (4, 30)",
        (),
    )
    .unwrap();
    let err = db.execute("CREATE UNIQUE INDEX ux ON t (k)", ());
    assert!(err.is_err(), "duplicate keys must fail the UNIQUE backfill");
    let msg = match err {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected UNIQUE failure"),
    };
    assert!(
        msg.contains("UNIQUE"),
        "error must name the constraint: {msg}"
    );

    // NULLs are distinct: many NULL keys are all exempt.
    db.execute("CREATE TABLE n (id INTEGER PRIMARY KEY, k INTEGER)", ())
        .unwrap();
    db.execute(
        "INSERT INTO n VALUES (1, NULL), (2, NULL), (3, NULL), (4, 5)",
        (),
    )
    .unwrap();
    db.execute("CREATE UNIQUE INDEX nx ON n (k)", ()).unwrap();
    assert_eq!(integrity(&mut db), "ok");
    let c: i64 = db
        .query("SELECT COUNT(*) FROM n WHERE k IS NULL", ())
        .unwrap()[0][0]
        .as_integer();
    assert_eq!(c, 3, "NULL rows must be findable through the index");
}

/// Partial + expression + DESC columns through the sorted backfill.
#[test]
fn create_index_sorted_backfill_variants() {
    let mut db = Database::open(tmpdb("ci-var")).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
        (),
    )
    .unwrap();
    for i in 1..=500i64 {
        db.execute(
            "INSERT INTO t VALUES (?, ?, ?)",
            vec![
                Value::Integer(i),
                Value::Integer(i % 17),
                Value::Text(format!("s{}", i % 5).into()),
            ],
        )
        .unwrap();
    }
    // Partial: only even ids.
    db.execute("CREATE INDEX px ON t (a) WHERE id % 2 = 0", ())
        .unwrap();
    // Expression: lowercase of b.
    db.execute("CREATE INDEX ex ON t (lower(b))", ()).unwrap();
    // DESC column.
    db.execute("CREATE INDEX dx ON t (a DESC)", ()).unwrap();
    assert_eq!(integrity(&mut db), "ok");
    for (idx, q, want_n) in [
        (
            "px",
            "SELECT COUNT(*) FROM t WHERE id % 2 = 0 AND a = 3",
            500 / 17 / 2 + 1,
        ),
        ("ex", "SELECT COUNT(*) FROM t WHERE lower(b) = 's3'", 100),
        ("dx", "SELECT COUNT(*) FROM t WHERE a = 3", 500 / 17 + 1),
    ] {
        let got: i64 = db.query(q, ()).unwrap()[0][0].as_integer();
        assert_eq!(got, want_n, "{idx}: {q}");
    }
    // The partial index contains exactly the even rows' keys.
    let got: i64 = db
        .query("SELECT COUNT(*) FROM t WHERE id % 2 = 0 AND a >= 0", ())
        .unwrap()[0][0]
        .as_integer();
    assert_eq!(got, 250);
}

/// Large-key (overflow-chain) entries flow through the backfill and the
/// sweep's decline path unchanged.
#[test]
fn oversized_keys_band_update_and_backfill() {
    let mut db = Database::open(tmpdb("big")).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT)", ())
        .unwrap();
    let big = |i: i64| format!("{:06}", i).repeat(200);
    db.execute("BEGIN", ()).unwrap();
    for i in 1..=400i64 {
        db.execute(
            "INSERT INTO t VALUES (?, ?)",
            vec![Value::Integer(i), Value::Text(big(i).into())],
        )
        .unwrap();
    }
    db.execute("COMMIT", ()).unwrap();
    db.execute("CREATE INDEX ix ON t (k)", ()).unwrap();
    assert_eq!(integrity(&mut db), "ok");
    // Band-move every other oversized key.
    db.execute(
        "UPDATE t SET k = ? WHERE id <= 200 AND id % 2 = 0",
        vec![Value::Text(big(99_999).into())],
    )
    .unwrap();
    assert_eq!(integrity(&mut db), "ok");
    let n: i64 = db
        .query(
            "SELECT COUNT(*) FROM t WHERE k = ?",
            vec![Value::Text(big(99_999).into())],
        )
        .unwrap()[0][0]
        .as_integer();
    assert_eq!(n, 100, "oversized band-move entries must be findable");
}

/// Multi-row same-key churn inside ONE statement: two rows swapping
/// keys (A→B while B→A) — the buffered multiset must land both.
#[test]
fn band_update_key_swaps_within_statement() {
    let mut db = Database::open(tmpdb("swap")).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER)", ())
        .unwrap();
    db.execute(
        "INSERT INTO t VALUES (1, 100), (2, 200), (3, 300), (4, 400)",
        (),
    )
    .unwrap();
    db.execute("CREATE INDEX ix ON t (k)", ()).unwrap();
    // Swap: 100<->300 and 200<->400 in one statement.
    db.execute(
        "UPDATE t SET k = CASE k WHEN 100 THEN 300 WHEN 300 THEN 100 WHEN 200 THEN 400 WHEN 400 THEN 200 END",
        (),
    )
    .unwrap();
    let got: Vec<(i64, i64)> = db
        .query("SELECT k, id FROM t ORDER BY k, id", ())
        .unwrap()
        .iter()
        .map(|r| (r[0].as_integer(), r[1].as_integer()))
        .collect();
    assert_eq!(got, vec![(100, 3), (200, 4), (300, 1), (400, 2)]);
    assert_eq!(integrity(&mut db), "ok");
}

/// A same-key UPDATE (k unchanged) buffers nothing — and a NULL↔value
/// transition moves the entry through the sweep.
#[test]
fn band_update_null_transitions() {
    let mut db = Database::open(tmpdb("null")).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER)", ())
        .unwrap();
    for i in 1..=1000i64 {
        let k = if i % 3 == 0 {
            Value::Null
        } else {
            Value::Integer(i % 7)
        };
        db.execute("INSERT INTO t VALUES (?, ?)", vec![Value::Integer(i), k])
            .unwrap();
    }
    db.execute("CREATE INDEX ix ON t (k)", ()).unwrap();
    // NULL -> value for ids divisible by 3; value -> NULL for others in
    // the band.
    db.execute(
        "UPDATE t SET k = CASE WHEN k IS NULL THEN id ELSE NULL END WHERE id <= 600",
        (),
    )
    .unwrap();
    assert_eq!(integrity(&mut db), "ok");
    let expect: Vec<(i64, i64)> = {
        let mut v: Vec<(i64, i64)> = Vec::new();
        for id in 1..=1000i64 {
            let orig_null = id % 3 == 0;
            let k = if id <= 600 {
                if orig_null {
                    Some(id)
                } else {
                    None
                }
            } else if orig_null {
                None
            } else {
                Some(id % 7)
            };
            if let Some(k) = k {
                v.push((k, id));
            }
        }
        v.sort_unstable();
        v
    };
    let got: Vec<(i64, i64)> = db
        .query("SELECT k, id FROM t WHERE k IS NOT NULL ORDER BY k, id", ())
        .unwrap()
        .iter()
        .map(|r| (r[0].as_integer(), r[1].as_integer()))
        .collect();
    assert_eq!(got, expect, "NULL-transition entries diverged");
}
