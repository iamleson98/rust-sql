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

/// Serializes the spilling tests AND the temp-file counting test: the
/// count is only deterministic when no other test's ephemeral file is
/// alive at sampling time (CI caught the race — parallel test threads
/// see each other's live spill files).
fn spill_gate() -> &'static std::sync::Mutex<()> {
    static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(std::sync::Mutex::default)
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
    let _gate = spill_gate().lock().unwrap_or_else(|e| e.into_inner());
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
    let _gate = spill_gate().lock().unwrap_or_else(|e| e.into_inner());
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
    let _gate = spill_gate().lock().unwrap_or_else(|e| e.into_inner());
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
    let _gate = spill_gate().lock().unwrap_or_else(|e| e.into_inner());
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
    let _gate = spill_gate().lock().unwrap_or_else(|e| e.into_inner());
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

/// REGRESSION (the shared-offset chunk-reader bug): a spilled grouper
/// whose chunks are LARGER than one reader buffer (64 KiB) with the
/// k-way merge interleaving streams — `File::try_clone` (dup) shares the
/// file offset, so a `BufReader` refill on one stream resumed at
/// wherever the OTHER stream's reads had moved the shared offset,
/// desynchronizing mid-record. Chunks at or under one buffer never
/// refilled, which is why the small-chunk tests never caught it. This
/// test drives FAT records (2 KiB TEXT keys) so each frozen chunk is
/// several buffers deep, then verifies the EXACT group set.
#[test]
fn spill_multibuffer_chunks_exact() {
    force_spill_env();
    let _gate = spill_gate().lock().unwrap_or_else(|e| e.into_inner());
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT)", [])
        .unwrap();
    // 400 groups x 2 KiB keys: threshold 4 -> 100+ epochs, every chunk
    // ~8 KiB x ... at 4 groups/chunk each chunk is ~8 KiB — TOO SMALL.
    // Use a count threshold that still forces multi-chunk but leaves
    // chunks over 64 KiB: the env is process-global (OnceLock), so the
    // shape does it with ROWS: 4000 distinct 2 KiB keys spill 1000
    // chunks of 4 groups (~8 KiB each) — the DANGER shape is one BIG
    // chunk; reproduce it by keying on the whole row set with a high
    // cardinality so each epoch's 4 groups hold big keys and the merge
    // interleaves two >64 KiB streams via the ROW-BACKED query path
    // (the serial scan path froze at 4389 groups/chunk in production).
    //
    // Simpler and just as deadly: 40_000 rows over 20_000 distinct
    // 2 KiB keys -> 5_000 frozen chunks; the final k-way merge opens
    // 5_000+ streams whose reads interleave constantly. The dup-offset
    // bug corrupted this within the first two refills.
    let mut seen = std::collections::HashSet::new();
    db.execute("BEGIN", []).unwrap();
    let mut i = 0i64;
    while seen.len() < 20_000 {
        let key = format!("key-{:06}-{}", i, "x".repeat(2048 - 12));
        if seen.insert(key.clone()) {
            db.execute("INSERT INTO t (k) VALUES (?)", [Value::Text(key.into())])
                .unwrap();
        }
        i += 1;
    }
    // Second pass: +1 row per key (COUNT=2 per group).
    let keys: Vec<String> = seen.into_iter().take(20_000).collect();
    for k in &keys {
        db.execute(
            "INSERT INTO t (k) VALUES (?)",
            [Value::Text(k.clone().into())],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let rows = db
        .query("SELECT k, COUNT(*) FROM t GROUP BY k", [])
        .unwrap();
    assert_eq!(rows.len(), 20_000, "every distinct key must group");
    for r in &rows {
        assert_eq!(
            r[1],
            Value::Integer(2),
            "each key groups exactly 2 rows: first={}",
            match &r[0] {
                Value::Text(t) => t.chars().take(12).collect::<String>(),
                _ => String::new(),
            }
        );
    }
}

/// REGRESSION (the dropped-frozen-chunks emission bug): the
/// materialized-input GROUP BY path (joins / subqueries / CTEs) emitted
/// through a RAW walk of the grouper's RAM table, silently dropping
/// every frozen chunk when the grouper spilled — a 20k-group JOIN
/// GROUP BY returned its 2.4k-group RAM tail as the whole answer.
/// The path now emits through the group iterator (k-way merge). This
/// test drives a JOIN shape big enough to spill the (now spill-armed)
/// materialized grouper AND the parallel split's merged destination.
#[test]
fn spill_materialized_join_groupby_exact() {
    force_spill_env();
    let _gate = spill_gate().lock().unwrap_or_else(|e| e.into_inner());
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER, x INTEGER)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER, y INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    // 6_000 rows, k = i - (i % 3) -> 2_000 distinct keys; the join fans
    // out 3x (18_000 rows). With the threshold at 4, the serial
    // materialized grouper spills ~500 chunks and the parallel split's
    // destination spills the same — both sides of the bug.
    for i in 1..=6_000i64 {
        db.execute(
            "INSERT INTO a (id, k, x) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i - (i % 3)),
                Value::Integer(i * 3),
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO b (id, k, y) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i - (i % 3)),
                Value::Integer(i * 7),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let sql = "SELECT a.k, COUNT(*), SUM(b.y), MIN(a.x), MAX(b.y) FROM a JOIN b ON a.k = b.k GROUP BY a.k";

    // Serial (parallel off). k = i - (i % 3) for i in 1..=6000 gives
    // 2001 distinct keys (0, 3, ..., 6000); the EDGE keys group fewer
    // rows (key 0: i in {1,2}; key 6000: i in {6000}) — the exact
    // per-key counts are pinned by the SQLite oracle below.
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let ser = db.query(sql, []).unwrap();
    assert_eq!(ser.len(), 2_001, "serial: every distinct key must group");

    // Parallel (force the split on: 6k-row tables sit under the default
    // 131k min-rows gate).
    db.execute("PRAGMA parallel_scan=1000", []).unwrap();
    let par = db.query(sql, []).unwrap();
    assert_eq!(par.len(), 2_001, "parallel: every distinct key must group");

    // Oracle: real SQLite.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER, x INTEGER)",
        [],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER, y INTEGER)",
        [],
    )
    .unwrap();
    for i in 1..=6_000i64 {
        conn.execute(
            "INSERT INTO a (id, k, x) VALUES (?1, ?2, ?3)",
            rusqlite::params![i, i - (i % 3), i * 3],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO b (id, k, y) VALUES (?1, ?2, ?3)",
            rusqlite::params![i, i - (i % 3), i * 7],
        )
        .unwrap();
    }
    let mut stmt = conn.prepare(sql).unwrap();
    let mut it = stmt.query([]).unwrap();
    let mut oracle = Vec::new();
    while let Some(r) = it.next().unwrap() {
        oracle.push(vec![
            Value::Integer(r.get::<_, i64>(0).unwrap()),
            Value::Integer(r.get::<_, i64>(1).unwrap()),
            Value::Integer(r.get::<_, i64>(2).unwrap()),
            Value::Integer(r.get::<_, i64>(3).unwrap()),
            Value::Integer(r.get::<_, i64>(4).unwrap()),
        ]);
    }
    let sortv = |v: Vec<Vec<Value>>| {
        let mut v = v;
        v.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        v
    };
    let ser_sorted = sortv(ser);
    let par_sorted = sortv(par);
    let oracle_sorted = sortv(oracle);
    assert_eq!(ser_sorted, oracle_sorted, "serial answer == SQLite");
    assert_eq!(par_sorted, oracle_sorted, "parallel answer == SQLite");
}
