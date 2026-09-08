//! Temp-store (ephemeral spill) tests for GROUP BY.
//!
//! The env var `RSQL_GROUP_SPILL_THRESHOLD` is read once per process at
//! the first spill arming, so every test that needs forced multi-chunk
//! spills runs inside ONE test function (the first one to execute sets
//! the env var before any query — `std::env::set_var` then a query in
//! the same thread; other test fns in this file only assert PRAGMA
//! semantics and cleanup, which are threshold-independent).

use rustqlite::{Database, Value};

fn force_spill_env() {
    // 4 groups per epoch: any GROUP BY over more than 4 distinct keys
    // freezes at least once; the interleaved keys below force several
    // freezes WITH recurring keys (the re-merge path).
    std::env::set_var("RSQL_GROUP_SPILL_THRESHOLD", "4");
}

fn rows_sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|a, b| {
        for (x, y) in a.iter().zip(b.iter()) {
            match x.cmp(y) {
                std::cmp::Ordering::Equal => continue,
                ord => return ord,
            }
        }
        a.len().cmp(&b.len())
    });
    rows
}

/// Build the interleaved-key table: keys 0..20 cycle 60 times, values
/// derived from (key, pass) so every aggregate is exactly computable.
fn build_table(db: &mut Database) {
    db.execute("CREATE TABLE t (k INT, g INT, v INT, s TEXT)", ())
        .unwrap();
    for pass in 0..60 {
        let txt = format!("k{}", pass % 20);
        let sql = format!(
            "INSERT INTO t VALUES ({k}, {g}, {v}, '{txt}')",
            k = pass % 20,
            g = pass % 3,
            v = pass % 20 * 10 + pass,
            txt = txt,
        );
        db.execute(&sql, ()).unwrap();
    }
}

/// The whole forced-spill matrix: every aggregate family, single and
/// multi keys, NULL keys, TEXT keys, DISTINCT, GROUP_CONCAT separators,
/// percentiles — each compared against the in-RAM reference
/// (`PRAGMA temp_store = MEMORY`) of the SAME query, row-set equal.
#[test]
fn group_by_spill_matches_ram_reference() {
    force_spill_env();
    let mut spilled = Database::open_in_memory().unwrap();
    build_table(&mut spilled);

    let mut reference = Database::open_in_memory().unwrap();
    build_table(&mut reference);
    reference.execute("PRAGMA temp_store = MEMORY", ()).unwrap();

    let queries: &[&str] = &[
        // single key, every numeric aggregate
        "SELECT k, COUNT(*), SUM(v), AVG(v), MIN(v), MAX(v), TOTAL(v) FROM t GROUP BY k",
        // multi-key
        "SELECT k, g, SUM(v), COUNT(*) FROM t GROUP BY k, g",
        // expression key
        "SELECT v / 10, COUNT(*) FROM t GROUP BY v / 10",
        // TEXT keys
        "SELECT s, COUNT(*), SUM(v) FROM t GROUP BY s",
        // GROUP_CONCAT with custom separator (order-sensitive merge)
        "SELECT k, GROUP_CONCAT(v, ';') FROM t GROUP BY k",
        // GROUP_CONCAT default separator
        "SELECT k, GROUP_CONCAT(v) FROM t GROUP BY k",
        // DISTINCT aggregate (set-union replay)
        "SELECT k, COUNT(DISTINCT g), SUM(DISTINCT v / 10) FROM t GROUP BY k",
        // NULL group keys
        "SELECT k + NULL, COUNT(*), SUM(v) FROM t GROUP BY k + NULL",
        // NULLs in aggregated values
        "SELECT g, SUM(NULL), COUNT(v), AVG(NULL) FROM t GROUP BY g",
        // percentile family (nums multiset merge)
        "SELECT g, MEDIAN(v), PERCENTILE_CONT(v, 25), STDDEV(v) FROM t GROUP BY g",
        // no-group-by guard: single aggregate row (no spill possible)
        "SELECT COUNT(*), SUM(v) FROM t",
        // empty input group-by
        "SELECT k, COUNT(*) FROM t WHERE v < 0 GROUP BY k",
        // HAVING over spilled groups
        "SELECT k, SUM(v) FROM t GROUP BY k HAVING SUM(v) > 100 ORDER BY k",
    ];

    for q in queries {
        let got = spilled.query(q, ()).unwrap();
        let want = reference.query(q, ()).unwrap();
        let got_s = rows_sorted(got);
        let want_s = rows_sorted(want);
        assert_eq!(
            format!("{:?}", got_s),
            format!("{:?}", want_s),
            "spilled GROUP BY diverged from RAM reference for: {q}"
        );
        // Non-empty for the queries that must return rows (guards the
        // "silently lost chunk" failure mode).
        if !q.contains("WHERE v < 0") {
            assert!(!got_s.is_empty(), "no rows for {q}");
        }
    }
}

/// The streaming (prepare/step) driver — the torture-S03 shape — must
/// produce the same rows as the buffered path under spill.
#[test]
fn group_by_spill_streaming_driver_matches_buffered() {
    force_spill_env();
    let mut db = Database::open_in_memory().unwrap();
    build_table(&mut db);

    let q = "SELECT k, COUNT(*), SUM(v) FROM t GROUP BY k";
    let buffered = rows_sorted(db.query(q, ()).unwrap());

    let mut streamed: Vec<Vec<Value>> = Vec::new();
    let mut stmt = db.prepare(q).unwrap();
    while let Ok(rustqlite::StepResult::Row) = stmt.step() {
        if let Some(row) = stmt.row() {
            streamed.push(row.to_vec());
        }
    }
    drop(stmt);

    assert_eq!(
        format!("{:?}", rows_sorted(streamed)),
        format!("{:?}", buffered)
    );
}

/// Parallel workers + spill + fold + final spill: force the parallel
/// split with a tiny min-rows threshold, then compare against the
/// serial RAM reference.
#[test]
fn group_by_spill_parallel_matches_serial() {
    force_spill_env();
    let mut par = Database::open_in_memory().unwrap();
    build_table(&mut par);
    // parallel_scan = 8 → split any scan above 8 estimated rows.
    par.execute("PRAGMA parallel_scan = 8", ()).unwrap();

    let mut serial = Database::open_in_memory().unwrap();
    build_table(&mut serial);
    serial.execute("PRAGMA parallel_scan = 0", ()).unwrap();
    serial.execute("PRAGMA temp_store = MEMORY", ()).unwrap();

    let queries: &[&str] = &[
        "SELECT k, COUNT(*), SUM(v), AVG(v), MIN(v), MAX(v) FROM t GROUP BY k",
        "SELECT k, g, SUM(v) FROM t GROUP BY k, g",
        "SELECT v / 10, COUNT(*) FROM t GROUP BY v / 10",
        "SELECT s, COUNT(*), SUM(v) FROM t GROUP BY s",
    ];
    for q in queries {
        let got = rows_sorted(par.query(q, ()).unwrap());
        let want = rows_sorted(serial.query(q, ()).unwrap());
        assert_eq!(
            format!("{:?}", got),
            format!("{:?}", want),
            "parallel spilled GROUP BY diverged for: {q}"
        );
    }
}

/// Spilled output order: key-sorted (SQLite's own ephemeral-sorter
/// order) — deterministic across runs and shapes above the threshold.
#[test]
fn group_by_spill_output_is_key_sorted() {
    force_spill_env();
    let mut db = Database::open_in_memory().unwrap();
    build_table(&mut db);
    let rows = db
        .query("SELECT k, COUNT(*) FROM t GROUP BY k", ())
        .unwrap();
    let keys: Vec<i64> = rows
        .iter()
        .filter_map(|r| match &r[0] {
            Value::Integer(i) => Some(*i),
            _ => None,
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "spilled GROUP BY output must be key-sorted");
    assert_eq!(keys.len(), 20);
}

/// `PRAGMA temp_store` round-trips and MEMORY disables arming.
#[test]
fn pragma_temp_store_round_trip() {
    let mut db = Database::open_in_memory().unwrap();
    let v = db.query("PRAGMA temp_store", ()).unwrap();
    assert_eq!(v[0][0], Value::Integer(0)); // default
    db.execute("PRAGMA temp_store = MEMORY", ()).unwrap();
    let v = db.query("PRAGMA temp_store", ()).unwrap();
    assert_eq!(v[0][0], Value::Integer(2));
    db.execute("PRAGMA temp_store = FILE", ()).unwrap();
    let v = db.query("PRAGMA temp_store", ()).unwrap();
    assert_eq!(v[0][0], Value::Integer(1));
    db.execute("PRAGMA temp_store = 0", ()).unwrap();
    let v = db.query("PRAGMA temp_store", ()).unwrap();
    assert_eq!(v[0][0], Value::Integer(0));
    // Out of range rejects.
    assert!(db.execute("PRAGMA temp_store = 3", ()).is_err());

    // MEMORY mode still answers correctly (the RAM reference contract).
    db.execute("PRAGMA temp_store = MEMORY", ()).unwrap();
    db.execute("CREATE TABLE m (a INT)", ()).unwrap();
    for i in 0..50 {
        db.execute(&format!("INSERT INTO m VALUES ({})", i % 7), ())
            .unwrap();
    }
    let rows = db
        .query("SELECT a, COUNT(*), SUM(a) FROM m GROUP BY a", ())
        .unwrap();
    assert_eq!(rows.len(), 7);
    let total: i64 = rows
        .iter()
        .filter_map(|r| match &r[1] {
            Value::Integer(c) => Some(*c),
            _ => None,
        })
        .sum();
    assert_eq!(total, 50);
}

/// Every ephemeral file is deleted when its grouper drops — no strays
/// in the OS temp directory after the queries complete.
#[test]
fn spill_files_are_cleaned_up() {
    force_spill_env();
    fn count_ephemeral() -> usize {
        std::fs::read_dir(std::env::temp_dir())
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .starts_with("rustqlite-ephemeral-")
                    })
                    .count()
            })
            .unwrap_or(0)
    }
    let before = count_ephemeral();
    let mut db = Database::open_in_memory().unwrap();
    build_table(&mut db);
    for _ in 0..5 {
        let _ = db
            .query("SELECT k, COUNT(*) FROM t GROUP BY k", ())
            .unwrap();
    }
    drop(db);
    let after = count_ephemeral();
    assert_eq!(
        before, after,
        "ephemeral spill files leaked (before {before}, after {after})"
    );
}
